//! The terminal emulation core: a VT100/VT220/xterm compatible screen model
//! driven by the `vte` escape-sequence parser.

pub mod cell;
pub mod charset;
pub mod color;

use std::collections::VecDeque;

use unicode_width::UnicodeWidthChar;
use vte::{Params, Perform};

pub use cell::{Cell, Color, Flags, Pen, Row};
use charset::Charset;
pub use color::{Palette, Rgb, Scheme};

/// How incoming bytes are decoded into characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Encoding {
    #[default]
    Utf8,
    /// IBM PC code page 437 (BBS / ANSI art).
    Cp437,
    /// ISO-8859-1.
    Latin1,
}

impl Encoding {
    pub const ALL: [Encoding; 3] = [Encoding::Utf8, Encoding::Cp437, Encoding::Latin1];
    pub fn name(self) -> &'static str {
        match self {
            Encoding::Utf8 => "UTF-8",
            Encoding::Cp437 => "CP437 (IBM PC)",
            Encoding::Latin1 => "ISO-8859-1",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseMode {
    #[default]
    Off,
    /// DECSET 9: press only.
    X10,
    /// DECSET 1000: press and release.
    Normal,
    /// DECSET 1002: also motion while a button is held.
    ButtonMotion,
    /// DECSET 1003: all motion.
    AnyMotion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Bar,
}

#[derive(Debug, Clone)]
pub struct Modes {
    pub autowrap: bool,
    pub origin: bool,
    pub insert: bool,
    /// LNM: LF also does CR, and Enter sends CR LF.
    pub newline: bool,
    pub app_cursor: bool,
    pub app_keypad: bool,
    pub cursor_visible: bool,
    pub cursor_blink: bool,
    pub reverse_video: bool,
    pub bracketed_paste: bool,
    pub mouse: MouseMode,
    pub mouse_sgr: bool,
    pub mouse_utf8: bool,
    pub focus_events: bool,
    pub alternate_scroll: bool,
}

impl Default for Modes {
    fn default() -> Self {
        Modes {
            autowrap: true,
            origin: false,
            insert: false,
            newline: false,
            app_cursor: false,
            app_keypad: false,
            cursor_visible: true,
            cursor_blink: true,
            reverse_video: false,
            bracketed_paste: false,
            mouse: MouseMode::Off,
            mouse_sgr: false,
            mouse_utf8: false,
            focus_events: false,
            alternate_scroll: true,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Cursor {
    pub x: usize,
    pub y: usize,
    pub pen: Pen,
    /// The cursor is past the last column; the next printable wraps.
    pub wrap_pending: bool,
    charsets: [Charset; 4],
    /// Which of G0..G3 is invoked into GL.
    gl: usize,
    /// A pending single shift (SS2/SS3).
    single_shift: Option<usize>,
}

#[derive(Debug, Clone)]
struct SavedCursor {
    cursor: Cursor,
    origin: bool,
    autowrap: bool,
}

/// The screen model. Implements [`vte::Perform`].
pub struct Term {
    cols: usize,
    rows: usize,
    grid: Vec<Row>,
    alt_grid: Vec<Row>,
    alt_active: bool,
    scrollback: VecDeque<Row>,
    pub scrollback_limit: usize,
    pub cursor: Cursor,
    saved: [Option<SavedCursor>; 2],
    top: usize,
    bottom: usize,
    tabs: Vec<bool>,
    pub modes: Modes,
    pub title: String,
    /// Bytes the terminal wants to send back to the host (reports).
    pub responses: Vec<u8>,
    /// Number of BEL characters received (the UI resets it).
    pub bells: u32,
    pub palette: Palette,
    base_palette: Palette,
    pub cursor_shape: CursorShape,
    /// ANSI.SYS / BBS quirks: ED 2 homes the cursor.
    pub bbs_mode: bool,
    last_char: Option<char>,
    /// Incremented whenever anything visible changes.
    pub generation: u64,
    /// Total number of lines ever pushed into the scrollback.
    pub lines_scrolled: u64,
    /// Clipboard contents set via OSC 52, for the UI to pick up.
    pub clipboard_request: Option<String>,
    pub answerback: String,
}

fn blank_grid(cols: usize, rows: usize) -> Vec<Row> {
    (0..rows).map(|_| Row::new(cols, &Pen::default())).collect()
}

/// Make `grid` exactly `rows` long, dropping rows at the bottom or adding blank ones.
fn fit_rows(grid: &mut Vec<Row>, rows: usize, cols: usize) {
    grid.truncate(rows);
    while grid.len() < rows {
        grid.push(Row::new(cols, &Pen::default()));
    }
}

fn default_tabs(cols: usize) -> Vec<bool> {
    (0..cols).map(|i| i % 8 == 0 && i != 0).collect()
}

impl Term {
    pub fn new(cols: usize, rows: usize, scheme: Scheme) -> Term {
        let cols = cols.max(2);
        let rows = rows.max(1);
        let palette = Palette::new(scheme);
        Term {
            cols,
            rows,
            grid: blank_grid(cols, rows),
            alt_grid: blank_grid(cols, rows),
            alt_active: false,
            scrollback: VecDeque::new(),
            scrollback_limit: 10_000,
            cursor: Cursor::default(),
            saved: [None, None],
            top: 0,
            bottom: rows - 1,
            tabs: default_tabs(cols),
            modes: Modes::default(),
            title: String::new(),
            responses: Vec::new(),
            bells: 0,
            palette: palette.clone(),
            base_palette: palette,
            cursor_shape: CursorShape::Block,
            bbs_mode: false,
            last_char: None,
            generation: 0,
            lines_scrolled: 0,
            clipboard_request: None,
            answerback: String::new(),
        }
    }

    pub fn cols(&self) -> usize {
        self.cols
    }
    pub fn rows(&self) -> usize {
        self.rows
    }
    pub fn alt_screen(&self) -> bool {
        self.alt_active
    }
    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    /// Row `y` of the visible screen.
    pub fn screen_row(&self, y: usize) -> &Row {
        &self.grid[y]
    }

    /// A line by absolute index: 0 is the oldest scrollback line, and
    /// `scrollback_len()` is the first screen row.
    pub fn line(&self, abs: usize) -> Option<&Row> {
        let sb = self.scrollback.len();
        if abs < sb {
            self.scrollback.get(abs)
        } else {
            self.grid.get(abs - sb)
        }
    }

    pub fn total_lines(&self) -> usize {
        self.scrollback.len() + self.rows
    }

    pub fn set_scheme(&mut self, scheme: Scheme) {
        self.base_palette = Palette::new(scheme);
        self.palette = self.base_palette.clone();
        self.generation += 1;
    }

    pub fn clear_scrollback(&mut self) {
        self.scrollback.clear();
        self.generation += 1;
    }

    /// Full reset (RIS).
    pub fn reset(&mut self) {
        let mut t = Term::new(self.cols, self.rows, Scheme::Vga);
        t.base_palette = self.base_palette.clone();
        t.palette = self.base_palette.clone();
        t.scrollback = std::mem::take(&mut self.scrollback);
        t.scrollback_limit = self.scrollback_limit;
        t.bbs_mode = self.bbs_mode;
        t.lines_scrolled = self.lines_scrolled;
        t.answerback = std::mem::take(&mut self.answerback);
        t.generation = self.generation + 1;
        *self = t;
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(2);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        if rows != self.rows {
            self.resize_rows(rows);
        }
        for row in self.grid.iter_mut().chain(self.alt_grid.iter_mut()) {
            row.resize(cols);
        }
        if cols != self.cols {
            let old = self.cols;
            self.tabs.resize(cols, false);
            for i in old..cols {
                self.tabs[i] = i % 8 == 0;
            }
        }
        self.cols = cols;
        self.rows = rows;
        self.top = 0;
        self.bottom = rows - 1;
        self.cursor.x = self.cursor.x.min(cols - 1);
        self.cursor.y = self.cursor.y.min(rows - 1);
        self.cursor.wrap_pending = false;
        for s in self.saved.iter_mut().flatten() {
            s.cursor.x = s.cursor.x.min(cols - 1);
            s.cursor.y = s.cursor.y.min(rows - 1);
        }
        self.generation += 1;
    }

    /// Change the number of rows on both screens.
    ///
    /// The primary screen keeps the line its cursor is on visible, spilling
    /// lines into scrollback when shrinking and pulling them back when
    /// growing. This happens even while the alternate screen is shown, so
    /// resizing inside vim or less doesn't eat the shell's screen. The
    /// alternate screen only keeps its own cursor on screen.
    fn resize_rows(&mut self, rows: usize) {
        let alt = self.alt_active;
        let mut primary = std::mem::take(if alt { &mut self.alt_grid } else { &mut self.grid });
        let anchor = if alt {
            match &self.saved[0] {
                Some(s) => s.cursor.y,
                None => primary.iter().rposition(|r| r.cells.iter().any(|c| c.ch != ' ')).unwrap_or(0),
            }
        } else {
            self.cursor.y
        };
        let shift = self.resize_primary(&mut primary, anchor, rows);
        let move_y = |y: &mut usize| *y = y.saturating_add_signed(shift).min(rows - 1);
        if let Some(s) = &mut self.saved[0] {
            move_y(&mut s.cursor.y);
        }
        if alt {
            self.alt_grid = primary;
            let from_top = (self.cursor.y + 1).saturating_sub(rows);
            self.grid.drain(..from_top);
            self.cursor.y -= from_top;
            fit_rows(&mut self.grid, rows, self.cols);
        } else {
            self.grid = primary;
            move_y(&mut self.cursor.y);
            fit_rows(&mut self.alt_grid, rows, self.cols);
        }
    }

    /// Resize the primary screen keeping row `anchor` visible. Returns how
    /// far the existing rows moved down (negative: up, into scrollback).
    fn resize_primary(&mut self, grid: &mut Vec<Row>, anchor: usize, rows: usize) -> isize {
        let mut shift = 0isize;
        if rows < grid.len() {
            // Drop rows below the anchor first; only push rows off the top
            // when the anchor would otherwise be hidden.
            let from_top = (anchor + 1).saturating_sub(rows);
            let gone: Vec<Row> = grid.drain(..from_top).collect();
            for row in gone {
                self.push_scrollback(row);
            }
            shift = -(from_top as isize);
        } else {
            while grid.len() < rows {
                let Some(mut row) = self.scrollback.pop_back() else { break };
                row.resize(self.cols);
                grid.insert(0, row);
                self.lines_scrolled = self.lines_scrolled.saturating_sub(1);
                shift += 1;
            }
        }
        fit_rows(grid, rows, self.cols);
        shift
    }

    // ----- primitive operations -------------------------------------------------

    fn push_scrollback(&mut self, row: Row) {
        // Count every line that leaves the screen, even with scrollback off,
        // so selections (anchored to absolute line numbers) follow the text.
        self.lines_scrolled += 1;
        if self.scrollback_limit == 0 {
            self.scrollback.clear();
            return;
        }
        while self.scrollback.len() >= self.scrollback_limit {
            self.scrollback.pop_front();
        }
        self.scrollback.push_back(row);
    }

    /// Scroll the region [top, bottom] up by `n` lines.
    fn scroll_up(&mut self, n: usize) {
        let (top, bottom) = (self.top, self.bottom);
        let n = n.min(bottom - top + 1);
        let pen = self.cursor.pen;
        let to_scrollback = top == 0 && !self.alt_active;
        if top == 0 && bottom == self.rows - 1 {
            // Fast path: whole screen.
            for row in self.grid.drain(..n).collect::<Vec<_>>() {
                if to_scrollback {
                    self.push_scrollback(row);
                }
            }
            for _ in 0..n {
                self.grid.push(Row::new(self.cols, &pen));
            }
        } else {
            for _ in 0..n {
                let row = self.grid.remove(top);
                if to_scrollback {
                    self.push_scrollback(row);
                }
                self.grid.insert(bottom, Row::new(self.cols, &pen));
            }
        }
    }

    /// Scroll the region [top, bottom] down by `n` lines.
    fn scroll_down(&mut self, n: usize) {
        let (top, bottom) = (self.top, self.bottom);
        let n = n.min(bottom - top + 1);
        let pen = self.cursor.pen;
        for _ in 0..n {
            self.grid.remove(bottom);
            self.grid.insert(top, Row::new(self.cols, &pen));
        }
    }

    fn linefeed(&mut self) {
        self.cursor.wrap_pending = false;
        if self.cursor.y == self.bottom {
            self.scroll_up(1);
        } else if self.cursor.y + 1 < self.rows {
            self.cursor.y += 1;
        }
    }

    fn reverse_index(&mut self) {
        self.cursor.wrap_pending = false;
        if self.cursor.y == self.top {
            self.scroll_down(1);
        } else if self.cursor.y > 0 {
            self.cursor.y -= 1;
        }
    }

    fn carriage_return(&mut self) {
        self.cursor.x = 0;
        self.cursor.wrap_pending = false;
    }

    /// Move to an absolute position (`y` is relative to the origin when DECOM is set).
    fn goto(&mut self, x: usize, y: usize) {
        let (min_y, max_y) = if self.modes.origin { (self.top, self.bottom) } else { (0, self.rows - 1) };
        self.cursor.x = x.min(self.cols - 1);
        self.cursor.y = (y + min_y).min(max_y);
        self.cursor.wrap_pending = false;
    }

    fn goto_row(&mut self, y: usize) {
        let x = self.cursor.x;
        self.goto(x, y);
    }

    fn origin_y(&self) -> usize {
        if self.modes.origin {
            self.cursor.y.saturating_sub(self.top)
        } else {
            self.cursor.y
        }
    }

    fn move_up(&mut self, n: usize) {
        let min = if self.cursor.y >= self.top { self.top } else { 0 };
        self.cursor.y = self.cursor.y.saturating_sub(n).max(min);
        self.cursor.wrap_pending = false;
    }

    fn move_down(&mut self, n: usize) {
        let max = if self.cursor.y <= self.bottom { self.bottom } else { self.rows - 1 };
        self.cursor.y = (self.cursor.y + n).min(max);
        self.cursor.wrap_pending = false;
    }

    fn move_left(&mut self, n: usize) {
        self.cursor.x = self.cursor.x.saturating_sub(n);
        self.cursor.wrap_pending = false;
    }

    fn move_right(&mut self, n: usize) {
        self.cursor.x = (self.cursor.x + n).min(self.cols - 1);
        self.cursor.wrap_pending = false;
    }

    fn tab_forward(&mut self, n: usize) {
        for _ in 0..n {
            let mut x = self.cursor.x + 1;
            while x < self.cols - 1 && !self.tabs[x] {
                x += 1;
            }
            self.cursor.x = x.min(self.cols - 1);
        }
        self.cursor.wrap_pending = false;
    }

    fn tab_backward(&mut self, n: usize) {
        for _ in 0..n {
            let mut x = self.cursor.x.saturating_sub(1);
            while x > 0 && !self.tabs[x] {
                x -= 1;
            }
            self.cursor.x = x;
        }
        self.cursor.wrap_pending = false;
    }

    /// Repair half-erased wide characters around column `x` of row `y`.
    /// If column `x` holds the right half of a wide character, blank the
    /// whole character (as xterm does) so shifting cells can't split it.
    fn erase_split_wide(&mut self, y: usize, x: usize) {
        let pen = self.cursor.pen;
        let row = &mut self.grid[y].cells;
        if x > 0 && x < row.len() && row[x].flags.contains(Flags::WIDE_SPACER) {
            row[x - 1] = Cell::blank(&pen);
            row[x] = Cell::blank(&pen);
        }
    }

    fn fix_wide(&mut self, y: usize, x: usize) {
        let pen = self.cursor.pen;
        let row = &mut self.grid[y].cells;
        if x < row.len()
            && row[x].flags.contains(Flags::WIDE_SPACER)
            && (x == 0 || !row[x - 1].flags.contains(Flags::WIDE))
        {
            row[x] = Cell::blank(&pen);
        }
        if x < row.len()
            && row[x].flags.contains(Flags::WIDE)
            && (x + 1 >= row.len() || !row[x + 1].flags.contains(Flags::WIDE_SPACER))
        {
            row[x] = Cell::blank(&pen);
        }
    }

    fn erase_cells(&mut self, y: usize, from: usize, to: usize) {
        let pen = self.cursor.pen;
        let to = to.min(self.cols);
        if from >= to {
            return;
        }
        self.grid[y].cells[from..to].fill(Cell::blank(&pen));
        if from > 0 {
            self.fix_wide(y, from - 1);
        }
        if to < self.cols {
            self.fix_wide(y, to);
        }
    }

    fn erase_display(&mut self, mode: u16) {
        let (x, y) = (self.cursor.x, self.cursor.y);
        let pen = self.cursor.pen;
        match mode {
            0 => {
                self.erase_cells(y, x, self.cols);
                self.grid[y].wrapped = false;
                for row in &mut self.grid[y + 1..] {
                    row.clear(&pen);
                }
            }
            1 => {
                for row in &mut self.grid[..y] {
                    row.clear(&pen);
                }
                self.erase_cells(y, 0, x + 1);
            }
            3 => {
                // xterm: ED 3 erases only the scrollback.
                self.scrollback.clear();
            }
            2 => {
                // Like xterm: move the old screen contents into scrollback when
                // clearing the primary screen, so nothing is lost.
                if !self.alt_active && self.scrollback_limit > 0 {
                    let last =
                        self.grid.iter().rposition(|r| r.cells.iter().any(|c| c.ch != ' ' || c.bg != Color::Default));
                    if let Some(last) = last {
                        let rows: Vec<Row> = self.grid[..=last].to_vec();
                        for row in rows {
                            self.push_scrollback(row);
                        }
                    }
                }
                for row in &mut self.grid {
                    row.clear(&pen);
                }
                if self.bbs_mode {
                    self.goto(0, 0);
                }
            }
            _ => {}
        }
    }

    fn erase_line(&mut self, mode: u16) {
        let (x, y) = (self.cursor.x, self.cursor.y);
        match mode {
            0 => {
                self.erase_cells(y, x, self.cols);
                self.grid[y].wrapped = false;
            }
            1 => self.erase_cells(y, 0, x + 1),
            2 => {
                let pen = self.cursor.pen;
                self.grid[y].clear(&pen);
            }
            _ => {}
        }
    }

    fn insert_chars(&mut self, n: usize) {
        let (x, y) = (self.cursor.x, self.cursor.y);
        let pen = self.cursor.pen;
        let cols = self.cols;
        let n = n.min(cols - x);
        self.erase_split_wide(y, x);
        let row = &mut self.grid[y].cells;
        row[x..].rotate_right(n);
        row[x..x + n].fill(Cell::blank(&pen));
        self.fix_wide(y, cols - 1);
        self.fix_wide(y, x + n);
        self.cursor.wrap_pending = false;
    }

    fn delete_chars(&mut self, n: usize) {
        let (x, y) = (self.cursor.x, self.cursor.y);
        let pen = self.cursor.pen;
        let cols = self.cols;
        let n = n.min(cols - x);
        self.erase_split_wide(y, x);
        let row = &mut self.grid[y].cells;
        row[x..].rotate_left(n);
        row[cols - n..].fill(Cell::blank(&pen));
        self.fix_wide(y, x);
        self.cursor.wrap_pending = false;
    }

    fn insert_lines(&mut self, n: usize) {
        let y = self.cursor.y;
        if y < self.top || y > self.bottom {
            return;
        }
        let pen = self.cursor.pen;
        let n = n.min(self.bottom - y + 1);
        for _ in 0..n {
            self.grid.remove(self.bottom);
            self.grid.insert(y, Row::new(self.cols, &pen));
        }
        self.cursor.x = 0;
        self.cursor.wrap_pending = false;
    }

    fn delete_lines(&mut self, n: usize) {
        let y = self.cursor.y;
        if y < self.top || y > self.bottom {
            return;
        }
        let pen = self.cursor.pen;
        let n = n.min(self.bottom - y + 1);
        for _ in 0..n {
            self.grid.remove(y);
            self.grid.insert(self.bottom, Row::new(self.cols, &pen));
        }
        self.cursor.x = 0;
        self.cursor.wrap_pending = false;
    }

    fn save_cursor(&mut self) {
        let idx = usize::from(self.alt_active);
        self.saved[idx] =
            Some(SavedCursor { cursor: self.cursor.clone(), origin: self.modes.origin, autowrap: self.modes.autowrap });
    }

    fn restore_cursor(&mut self) {
        let idx = usize::from(self.alt_active);
        match self.saved[idx].clone() {
            Some(s) => {
                self.cursor = s.cursor;
                self.modes.origin = s.origin;
                self.modes.autowrap = s.autowrap;
                self.cursor.x = self.cursor.x.min(self.cols - 1);
                self.cursor.y = self.cursor.y.min(self.rows - 1);
            }
            None => {
                self.cursor.x = 0;
                self.cursor.y = 0;
                self.cursor.pen = Pen::default();
                self.cursor.wrap_pending = false;
            }
        }
    }

    fn enter_alt_screen(&mut self, clear: bool) {
        if !self.alt_active {
            std::mem::swap(&mut self.grid, &mut self.alt_grid);
            self.alt_active = true;
        }
        if clear {
            let pen = self.cursor.pen;
            for row in &mut self.grid {
                row.clear(&pen);
            }
        }
    }

    fn leave_alt_screen(&mut self) {
        if self.alt_active {
            std::mem::swap(&mut self.grid, &mut self.alt_grid);
            self.alt_active = false;
        }
    }

    fn set_scroll_region(&mut self, top: usize, bottom: usize) {
        let bottom = bottom.min(self.rows - 1);
        if top < bottom {
            self.top = top;
            self.bottom = bottom;
            self.goto(0, 0);
        }
    }

    fn soft_reset(&mut self) {
        self.modes.insert = false;
        self.modes.origin = false;
        self.modes.autowrap = true;
        self.modes.app_cursor = false;
        self.modes.app_keypad = false;
        self.modes.cursor_visible = true;
        self.top = 0;
        self.bottom = self.rows - 1;
        self.cursor.pen = Pen::default();
        self.cursor.charsets = [Charset::Ascii; 4];
        self.cursor.gl = 0;
        self.cursor.single_shift = None;
        self.saved = [None, None];
    }

    fn put_char(&mut self, c: char) {
        let width = match c.width() {
            Some(w) => w,
            None => return,
        };
        if width == 0 {
            // Combining marks are not stored; they are rare in terminal use.
            return;
        }
        if self.cursor.wrap_pending && self.modes.autowrap {
            self.grid[self.cursor.y].wrapped = true;
            self.carriage_return();
            self.linefeed();
        }
        if width == 2 && self.cursor.x + 1 >= self.cols {
            if self.modes.autowrap {
                let pen = self.cursor.pen;
                let (x, y) = (self.cursor.x, self.cursor.y);
                self.grid[y].cells[x] = Cell::blank(&pen);
                self.grid[y].wrapped = true;
                self.carriage_return();
                self.linefeed();
            } else {
                self.cursor.x = self.cols.saturating_sub(2);
            }
        }
        if self.modes.insert {
            self.insert_chars(width);
        }
        let (x, y) = (self.cursor.x, self.cursor.y);
        let pen = self.cursor.pen;
        // Overwriting half of an existing wide character blanks the other half.
        {
            let row = &mut self.grid[y].cells;
            if row[x].flags.contains(Flags::WIDE_SPACER) && x > 0 {
                row[x - 1] = Cell::blank(&pen);
            }
            let end = x + width;
            if end < row.len() && row[end].flags.contains(Flags::WIDE_SPACER) {
                row[end] = Cell::blank(&pen);
            }
            let mut flags = pen.flags;
            if width == 2 {
                flags.insert(Flags::WIDE);
            }
            row[x] = Cell { ch: c, fg: pen.fg, bg: pen.bg, flags };
            if width == 2 {
                let mut sflags = pen.flags;
                sflags.insert(Flags::WIDE_SPACER);
                row[x + 1] = Cell { ch: ' ', fg: pen.fg, bg: pen.bg, flags: sflags };
            }
        }
        if x + width >= self.cols {
            self.cursor.x = self.cols - 1;
            self.cursor.wrap_pending = true;
        } else {
            self.cursor.x = x + width;
        }
        self.last_char = Some(c);
    }

    // ----- SGR -----------------------------------------------------------------

    fn sgr(&mut self, params: &Params) {
        let pen = &mut self.cursor.pen;
        if params.is_empty() {
            *pen = Pen::default();
            return;
        }
        let mut it = params.iter();
        while let Some(p) = it.next() {
            match p[0] {
                0 => *pen = Pen::default(),
                1 => pen.flags.insert(Flags::BOLD),
                2 => pen.flags.insert(Flags::DIM),
                3 => pen.flags.insert(Flags::ITALIC),
                4 => {
                    pen.flags.remove(Flags::UNDERLINE);
                    pen.flags.remove(Flags::DOUBLE_UNDERLINE);
                    match p.get(1).copied() {
                        Some(0) => {}
                        Some(2) => pen.flags.insert(Flags::DOUBLE_UNDERLINE),
                        _ => pen.flags.insert(Flags::UNDERLINE),
                    }
                }
                5 | 6 => pen.flags.insert(Flags::BLINK),
                7 => pen.flags.insert(Flags::INVERSE),
                8 => pen.flags.insert(Flags::HIDDEN),
                9 => pen.flags.insert(Flags::STRIKE),
                21 => pen.flags.insert(Flags::DOUBLE_UNDERLINE),
                22 => {
                    pen.flags.remove(Flags::BOLD);
                    pen.flags.remove(Flags::DIM);
                }
                23 => pen.flags.remove(Flags::ITALIC),
                24 => {
                    pen.flags.remove(Flags::UNDERLINE);
                    pen.flags.remove(Flags::DOUBLE_UNDERLINE);
                }
                25 => pen.flags.remove(Flags::BLINK),
                27 => pen.flags.remove(Flags::INVERSE),
                28 => pen.flags.remove(Flags::HIDDEN),
                29 => pen.flags.remove(Flags::STRIKE),
                30..=37 => pen.fg = Color::Indexed((p[0] - 30) as u8),
                38 => {
                    if let Some(c) = extended_color(p, &mut it) {
                        pen.fg = c;
                    }
                }
                39 => pen.fg = Color::Default,
                40..=47 => pen.bg = Color::Indexed((p[0] - 40) as u8),
                48 => {
                    if let Some(c) = extended_color(p, &mut it) {
                        pen.bg = c;
                    }
                }
                49 => pen.bg = Color::Default,
                53 => pen.flags.insert(Flags::OVERLINE),
                55 => pen.flags.remove(Flags::OVERLINE),
                58 => {
                    // Underline color: parsed and ignored.
                    let _ = extended_color(p, &mut it);
                }
                90..=97 => pen.fg = Color::Indexed((p[0] - 90 + 8) as u8),
                100..=107 => pen.bg = Color::Indexed((p[0] - 100 + 8) as u8),
                _ => {}
            }
        }
    }

    // ----- modes -----------------------------------------------------------------

    fn set_private_mode(&mut self, mode: u16, on: bool) {
        match mode {
            1 => self.modes.app_cursor = on,
            3 => {
                // DECCOLM: we don't change the window size, but clear like a VT100.
                self.erase_display(2);
                self.set_scroll_region(0, self.rows - 1);
                self.goto(0, 0);
            }
            5 => self.modes.reverse_video = on,
            6 => {
                self.modes.origin = on;
                self.goto(0, 0);
            }
            7 => self.modes.autowrap = on,
            9 => self.modes.mouse = if on { MouseMode::X10 } else { MouseMode::Off },
            12 => self.modes.cursor_blink = on,
            25 => self.modes.cursor_visible = on,
            47 | 1047 => {
                if on {
                    self.enter_alt_screen(mode == 1047);
                } else {
                    if mode == 1047 && self.alt_active {
                        let pen = self.cursor.pen;
                        for row in &mut self.grid {
                            row.clear(&pen);
                        }
                    }
                    self.leave_alt_screen();
                }
            }
            1048 => {
                if on {
                    self.save_cursor()
                } else {
                    self.restore_cursor()
                }
            }
            1049 => {
                if on {
                    self.save_cursor();
                    self.enter_alt_screen(true);
                    // The saved cursor belongs to the primary screen.
                    self.saved[1] = self.saved[0].clone();
                } else {
                    self.leave_alt_screen();
                    self.restore_cursor();
                }
            }
            1000 => self.modes.mouse = if on { MouseMode::Normal } else { MouseMode::Off },
            1002 => self.modes.mouse = if on { MouseMode::ButtonMotion } else { MouseMode::Off },
            1003 => self.modes.mouse = if on { MouseMode::AnyMotion } else { MouseMode::Off },
            1004 => self.modes.focus_events = on,
            1005 => self.modes.mouse_utf8 = on,
            1006 => self.modes.mouse_sgr = on,
            1007 => self.modes.alternate_scroll = on,
            2004 => self.modes.bracketed_paste = on,
            _ => log::debug!("unhandled DEC private mode {mode} = {on}"),
        }
    }

    fn private_mode_state(&self, mode: u16) -> Option<bool> {
        Some(match mode {
            1 => self.modes.app_cursor,
            5 => self.modes.reverse_video,
            6 => self.modes.origin,
            7 => self.modes.autowrap,
            9 => self.modes.mouse == MouseMode::X10,
            12 => self.modes.cursor_blink,
            25 => self.modes.cursor_visible,
            47 | 1047 | 1049 => self.alt_active,
            1000 => self.modes.mouse == MouseMode::Normal,
            1002 => self.modes.mouse == MouseMode::ButtonMotion,
            1003 => self.modes.mouse == MouseMode::AnyMotion,
            1004 => self.modes.focus_events,
            1005 => self.modes.mouse_utf8,
            1006 => self.modes.mouse_sgr,
            1007 => self.modes.alternate_scroll,
            2004 => self.modes.bracketed_paste,
            _ => return None,
        })
    }

    fn respond(&mut self, s: &str) {
        self.responses.extend_from_slice(s.as_bytes());
    }

    fn osc_color_reply(&mut self, code: &str, c: Rgb, bell: bool) {
        let term = if bell { "\x07" } else { "\x1b\\" };
        let s = format!("\x1b]{code};{}{term}", c.to_x11());
        self.respond(&s);
    }
}

/// Parse the color following SGR 38/48/58, in either `38;5;n` / `38;2;r;g;b`
/// form or the colon sub-parameter form `38:5:n` / `38:2::r:g:b`.
fn extended_color<'a>(p: &[u16], it: &mut impl Iterator<Item = &'a [u16]>) -> Option<Color> {
    let clamp = |v: u16| v.min(255) as u8;
    if p.len() > 1 {
        return match p[1] {
            5 => p.get(2).map(|&n| Color::Indexed(clamp(n))),
            2 => {
                let rgb = if p.len() >= 6 { &p[p.len() - 3..] } else { p.get(2..5)? };
                Some(Color::Rgb(clamp(rgb[0]), clamp(rgb[1]), clamp(rgb[2])))
            }
            _ => None,
        };
    }
    match it.next()?[0] {
        5 => Some(Color::Indexed(clamp(it.next()?[0]))),
        2 => {
            let r = it.next()?[0];
            let g = it.next()?[0];
            let b = it.next()?[0];
            Some(Color::Rgb(clamp(r), clamp(g), clamp(b)))
        }
        _ => None,
    }
}

/// Collect the first sub-parameter of each parameter.
fn flat_params(params: &Params) -> ([u16; 16], usize) {
    let mut out = [0u16; 16];
    let mut n = 0;
    for p in params.iter() {
        if n == out.len() {
            break;
        }
        out[n] = p[0];
        n += 1;
    }
    (out, n)
}

fn decode_base64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for b in s.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\r' | b'\n' => continue,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

impl Perform for Term {
    fn print(&mut self, c: char) {
        let c = if (c as u32) < 0x80 {
            let set = match self.cursor.single_shift.take() {
                Some(g) => self.cursor.charsets[g],
                None => self.cursor.charsets[self.cursor.gl],
            };
            set.map(c)
        } else {
            c
        };
        self.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x05 => {
                let a = self.answerback.clone();
                self.respond(&a);
            }
            0x07 => self.bells = self.bells.saturating_add(1),
            0x08 => {
                if self.cursor.wrap_pending {
                    self.cursor.wrap_pending = false;
                } else {
                    self.move_left(1);
                }
            }
            0x09 => self.tab_forward(1),
            0x0A..=0x0C => {
                self.linefeed();
                if self.modes.newline {
                    self.carriage_return();
                }
            }
            0x0D => self.carriage_return(),
            0x0E => self.cursor.gl = 1,
            0x0F => self.cursor.gl = 0,
            0x84 => self.linefeed(),
            0x85 => {
                self.linefeed();
                self.carriage_return();
            }
            0x88 => {
                let x = self.cursor.x;
                self.tabs[x] = true;
            }
            0x8D => self.reverse_index(),
            0x8E => self.cursor.single_shift = Some(2),
            0x8F => self.cursor.single_shift = Some(3),
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        let (p, np) = flat_params(params);
        // Parameter `i`, with 0/missing meaning `default`.
        let arg = |i: usize, default: usize| -> usize {
            if i < np && p[i] != 0 {
                usize::from(p[i])
            } else {
                default
            }
        };
        let raw = |i: usize| -> u16 {
            if i < np {
                p[i]
            } else {
                0
            }
        };

        match (intermediates, action) {
            ([], '@') => self.insert_chars(arg(0, 1)),
            ([], 'A') => self.move_up(arg(0, 1)),
            ([], 'B') | ([], 'e') => self.move_down(arg(0, 1)),
            ([], 'C') | ([], 'a') => self.move_right(arg(0, 1)),
            ([], 'D') => self.move_left(arg(0, 1)),
            ([], 'E') => {
                self.move_down(arg(0, 1));
                self.carriage_return();
            }
            ([], 'F') => {
                self.move_up(arg(0, 1));
                self.carriage_return();
            }
            ([], 'G') | ([], '`') => {
                self.cursor.x = (arg(0, 1) - 1).min(self.cols - 1);
                self.cursor.wrap_pending = false;
            }
            ([], 'H') | ([], 'f') => self.goto(arg(1, 1) - 1, arg(0, 1) - 1),
            ([], 'I') => self.tab_forward(arg(0, 1)),
            ([], 'J') | ([b'?'], 'J') => self.erase_display(raw(0)),
            ([], 'K') | ([b'?'], 'K') => self.erase_line(raw(0)),
            ([], 'L') => self.insert_lines(arg(0, 1)),
            ([], 'M') => self.delete_lines(arg(0, 1)),
            ([], 'P') => self.delete_chars(arg(0, 1)),
            ([], 'S') => self.scroll_up(arg(0, 1)),
            ([], 'T') if np <= 1 => self.scroll_down(arg(0, 1)),
            ([], 'X') => {
                let (x, y) = (self.cursor.x, self.cursor.y);
                self.erase_cells(y, x, x + arg(0, 1));
                self.cursor.wrap_pending = false;
            }
            ([], 'Z') => self.tab_backward(arg(0, 1)),
            ([], 'b') => {
                if let Some(c) = self.last_char {
                    // Capped at a line: more is rarely useful and lets a few
                    // bytes of input cost unbounded CPU.
                    for _ in 0..arg(0, 1).min(self.cols) {
                        self.put_char(c);
                    }
                }
            }
            ([], 'c') if raw(0) == 0 => self.respond("\x1b[?62;1;6;22c"),
            ([b'>'], 'c') if raw(0) == 0 => self.respond("\x1b[>1;10;0c"),
            ([], 'd') => self.goto_row(arg(0, 1) - 1),
            ([], 'g') => match raw(0) {
                0 => {
                    let x = self.cursor.x;
                    self.tabs[x] = false;
                }
                3 => self.tabs.fill(false),
                _ => {}
            },
            ([], 'h') | ([], 'l') => {
                let on = action == 'h';
                for &m in &p[..np] {
                    match m {
                        4 => self.modes.insert = on,
                        20 => self.modes.newline = on,
                        _ => {}
                    }
                }
            }
            ([b'?'], 'h') | ([b'?'], 'l') => {
                let on = action == 'h';
                for &m in &p[..np] {
                    self.set_private_mode(m, on);
                }
            }
            ([b'?'], 's') => {
                // XTSAVE: rarely needed; ignore.
            }
            ([], 'm') => self.sgr(params),
            ([], 'n') => match raw(0) {
                5 => self.respond("\x1b[0n"),
                6 => {
                    let s = format!("\x1b[{};{}R", self.origin_y() + 1, self.cursor.x + 1);
                    self.respond(&s);
                }
                _ => {}
            },
            ([b'?'], 'n') if raw(0) == 6 => {
                let s = format!("\x1b[?{};{}R", self.origin_y() + 1, self.cursor.x + 1);
                self.respond(&s);
            }
            ([], 'r') => {
                let top = arg(0, 1) - 1;
                let bottom = arg(1, self.rows) - 1;
                self.set_scroll_region(top, bottom);
            }
            ([], 's') => self.save_cursor(),
            ([], 'u') => self.restore_cursor(),
            // Window ops: only 18 (report size). 21 (report title) is
            // deliberately unsupported: echoing a host-settable string back
            // as input is an injection vector.
            ([], 't') if raw(0) == 18 => {
                let s = format!("\x1b[8;{};{}t", self.rows, self.cols);
                self.respond(&s);
            }
            ([b' '], 'q') => {
                let v = raw(0);
                self.cursor_shape = match v {
                    0..=2 => CursorShape::Block,
                    3 | 4 => CursorShape::Underline,
                    _ => CursorShape::Bar,
                };
                if v > 0 {
                    self.modes.cursor_blink = v % 2 == 1;
                }
            }
            ([b'!'], 'p') => self.soft_reset(),
            ([b'?', b'$'], 'p') => {
                let m = raw(0);
                let state = match self.private_mode_state(m) {
                    Some(true) => 1,
                    Some(false) => 2,
                    None => 0,
                };
                let s = format!("\x1b[?{m};{state}$y");
                self.respond(&s);
            }
            ([b'$'], 'p') => {
                let m = raw(0);
                let state = match m {
                    4 => 2 - u8::from(self.modes.insert),
                    20 => 2 - u8::from(self.modes.newline),
                    _ => 0,
                };
                let s = format!("\x1b[{m};{state}$y");
                self.respond(&s);
            }
            _ => log::debug!("unhandled CSI {:?} {:?} {}", &p[..np], intermediates, action),
        }
        self.generation += 1;
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        match (intermediates, byte) {
            ([], b'7') => self.save_cursor(),
            ([], b'8') => self.restore_cursor(),
            ([], b'D') => self.linefeed(),
            ([], b'E') => {
                self.linefeed();
                self.carriage_return();
            }
            ([], b'H') => {
                let x = self.cursor.x;
                self.tabs[x] = true;
            }
            ([], b'M') => self.reverse_index(),
            ([], b'N') => self.cursor.single_shift = Some(2),
            ([], b'O') => self.cursor.single_shift = Some(3),
            ([], b'n') => self.cursor.gl = 2,
            ([], b'o') => self.cursor.gl = 3,
            ([], b'c') => self.reset(),
            ([], b'=') => self.modes.app_keypad = true,
            ([], b'>') => self.modes.app_keypad = false,
            ([b'#'], b'8') => {
                // DECALN: fill the screen with 'E'.
                for row in &mut self.grid {
                    for c in &mut row.cells {
                        *c = Cell { ch: 'E', ..Cell::default() };
                    }
                }
                self.top = 0;
                self.bottom = self.rows - 1;
                self.goto(0, 0);
            }
            ([i], f) if matches!(i, b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/') => {
                let g = match i {
                    b'(' => 0,
                    b')' | b'-' => 1,
                    b'*' | b'.' => 2,
                    _ => 3,
                };
                self.cursor.charsets[g] = match f {
                    b'0' => Charset::DecSpecial,
                    b'A' => Charset::Uk,
                    _ => Charset::Ascii,
                };
            }
            _ => log::debug!("unhandled ESC {:?} {}", intermediates, byte as char),
        }
        self.generation += 1;
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        let Some(code) = params.first().and_then(|p| std::str::from_utf8(p).ok()) else {
            return;
        };
        let text = |i: usize| params.get(i).map(|p| String::from_utf8_lossy(p).into_owned()).unwrap_or_default();
        match code {
            "0" | "2" => {
                let mut t = text(1);
                for extra in params.iter().skip(2) {
                    t.push(';');
                    t.push_str(&String::from_utf8_lossy(extra));
                }
                self.title = t;
            }
            "1" => {}
            "4" => {
                let mut i = 1;
                while i + 1 < params.len() {
                    let idx: Option<usize> = text(i).parse().ok();
                    let spec = text(i + 1);
                    if let Some(idx) = idx.filter(|&n| n < 256) {
                        if spec == "?" {
                            let c = self.palette.colors[idx];
                            self.osc_color_reply(&format!("4;{idx}"), c, bell_terminated);
                        } else if let Some(c) = Rgb::from_x11(&spec) {
                            self.palette.colors[idx] = c;
                        }
                    }
                    i += 2;
                }
            }
            "10" | "11" | "12" => {
                let spec = text(1);
                let slot = match code {
                    "10" => &mut self.palette.fg,
                    "11" => &mut self.palette.bg,
                    _ => &mut self.palette.cursor,
                };
                if spec == "?" {
                    let c = *slot;
                    self.osc_color_reply(code, c, bell_terminated);
                } else if let Some(c) = Rgb::from_x11(&spec) {
                    *slot = c;
                }
            }
            "104" => {
                if params.len() <= 1 {
                    self.palette.colors = self.base_palette.colors;
                } else {
                    for i in 1..params.len() {
                        if let Ok(idx) = text(i).parse::<usize>() {
                            if idx < 256 {
                                self.palette.colors[idx] = self.base_palette.colors[idx];
                            }
                        }
                    }
                }
            }
            "110" => self.palette.fg = self.base_palette.fg,
            "111" => self.palette.bg = self.base_palette.bg,
            "112" => self.palette.cursor = self.base_palette.cursor,
            "52" => {
                // Only setting the clipboard is supported; reading it back
                // would let remote hosts snoop on the user.
                let data = text(2);
                if data != "?" {
                    if let Some(bytes) = decode_base64(&data) {
                        self.clipboard_request = Some(String::from_utf8_lossy(&bytes).into_owned());
                    }
                }
            }
            _ => {}
        }
        self.generation += 1;
    }
}

/// Longest OSC string kept (titles, palette, OSC 52 clipboard).
const OSC_MAX: usize = 256 * 1024;

/// The parser plus the screen it drives.
pub struct Emulator {
    /// Fixed-size OSC buffer: longer OSC strings are truncated, so a
    /// hostile stream can't grow memory without bound.
    parser: Box<vte::Parser<OSC_MAX>>,
    pub term: Term,
    pub encoding: Encoding,
    scratch: Vec<u8>,
}

impl Emulator {
    pub fn new(cols: usize, rows: usize, scheme: Scheme) -> Emulator {
        Emulator {
            parser: Box::new(vte::Parser::new_with_size()),
            term: Term::new(cols, rows, scheme),
            encoding: Encoding::Utf8,
            scratch: Vec::new(),
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        match self.encoding {
            Encoding::Utf8 => self.parser.advance(&mut self.term, bytes),
            Encoding::Cp437 => {
                self.scratch.clear();
                charset::cp437_to_utf8(bytes, &mut self.scratch);
                self.parser.advance(&mut self.term, &self.scratch);
            }
            Encoding::Latin1 => {
                self.scratch.clear();
                charset::latin1_to_utf8(bytes, &mut self.scratch);
                self.parser.advance(&mut self.term, &self.scratch);
            }
        }
        self.term.generation += 1;
    }

    /// Encode text typed or pasted by the user for the wire.
    pub fn encode_input(&self, s: &str, out: &mut Vec<u8>) {
        match self.encoding {
            Encoding::Utf8 => out.extend_from_slice(s.as_bytes()),
            Encoding::Cp437 => out.extend(s.chars().map(|c| charset::char_to_cp437(c).unwrap_or(b'?'))),
            Encoding::Latin1 => out.extend(s.chars().map(|c| if (c as u32) < 256 { c as u8 } else { b'?' })),
        }
    }
}

#[cfg(test)]
mod tests;
