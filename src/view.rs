//! The terminal widget: paints the emulator grid and turns keyboard/mouse
//! input into bytes for the host.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use egui::text::{LayoutJob, TextFormat};
use egui::{pos2, vec2, Color32, Event, FontId, Key, PointerButton, Rect, Sense, Stroke, StrokeKind};

use crate::boxdraw;
use crate::config::Settings;
use crate::input::{self, KeyOptions, MouseAction};
use crate::term::{Cell, Color, CursorShape, Emulator, Flags, MouseMode, Rgb, Term};

/// A point in the scrollback-inclusive line space. `line` is an absolute line
/// number that stays stable as lines scroll (see `Term::lines_scrolled`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Point {
    line: i64,
    col: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectMode {
    Char,
    Word,
    Line,
}

#[derive(Debug, Clone, Copy)]
struct Selection {
    anchor: Point,
    head: Point,
    mode: SelectMode,
}

impl Selection {
    fn ordered(&self) -> (Point, Point) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

/// What the widget wants the app to do after a frame.
#[derive(Default)]
pub struct ViewOutput {
    pub send: Vec<u8>,
    pub copy: Option<String>,
    /// New grid size: (cols, rows, pixel width, pixel height).
    pub resized: Option<(u16, u16, u16, u16)>,
}

pub struct TermView {
    /// Lines scrolled back from the live screen.
    pub scroll: usize,
    last_lines_scrolled: u64,
    selection: Option<Selection>,
    dragging: bool,
    mouse_button_down: Option<u8>,
    last_report_cell: Option<(usize, usize)>,
    last_click: Option<(Instant, Point, u8)>,
    wheel_accum: f32,
    glyph_ok: HashMap<char, bool>,
    glyph_font_size: f32,
    cell: egui::Vec2,
    advance: f32,
    row_height: f32,
    epoch: Instant,
    bell_until: Option<Instant>,
    last_bells: u32,
    pub has_focus: bool,
    window_focused: bool,
    suppress_text: bool,
}

impl Default for TermView {
    fn default() -> Self {
        TermView {
            scroll: 0,
            last_lines_scrolled: 0,
            selection: None,
            dragging: false,
            mouse_button_down: None,
            last_report_cell: None,
            last_click: None,
            wheel_accum: 0.0,
            glyph_ok: HashMap::new(),
            glyph_font_size: 0.0,
            cell: vec2(8.0, 16.0),
            advance: 8.0,
            row_height: 16.0,
            epoch: Instant::now(),
            bell_until: None,
            last_bells: 0,
            has_focus: false,
            window_focused: true,
            suppress_text: false,
        }
    }
}

fn c32(c: Rgb) -> Color32 {
    Color32::from_rgb(c.0, c.1, c.2)
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || "-_.~/:@%+#?&=".contains(c)
}

/// Resolved colors and style for one cell.
#[derive(Clone, Copy, PartialEq)]
struct Style {
    fg: Color32,
    bg: Color32,
    underline: u8,
    strike: bool,
    italic: bool,
    hidden: bool,
}

impl TermView {
    /// Grid size that fits in `size` points with the current font.
    pub fn grid_for(&self, size: egui::Vec2) -> (usize, usize) {
        let cols = (size.x / self.cell.x).floor().max(2.0) as usize;
        let rows = (size.y / self.cell.y).floor().max(1.0) as usize;
        (cols, rows)
    }

    pub fn cell_size(&self) -> egui::Vec2 {
        self.cell
    }

    fn update_metrics(&mut self, ctx: &egui::Context, font: &FontId) {
        let ppp = ctx.pixels_per_point();
        let (advance, row_height) = ctx.fonts_mut(|f| (f.glyph_width(font, 'M'), f.row_height(font)));
        let snap = |v: f32| ((v * ppp).round() / ppp).max(1.0 / ppp);
        self.advance = advance;
        self.row_height = row_height;
        self.cell = vec2(snap(advance), snap(row_height.max(font.size * 1.15)));
        if (self.glyph_font_size - font.size).abs() > f32::EPSILON {
            self.glyph_ok.clear();
            self.glyph_font_size = font.size;
        }
    }

    fn resolve(&self, term: &Term, cell: &Cell, settings: &Settings, selected: bool, blink_on: bool) -> Style {
        let pal = &term.palette;
        let flags = cell.flags;
        let bold = flags.contains(Flags::BOLD);
        let mut fg = match cell.fg {
            Color::Default => {
                if bold && settings.bold_is_bright {
                    pal.colors[15]
                } else {
                    pal.fg
                }
            }
            Color::Indexed(i) => {
                let i = if bold && settings.bold_is_bright && i < 8 { i + 8 } else { i };
                pal.colors[i as usize]
            }
            Color::Rgb(r, g, b) => Rgb(r, g, b),
        };
        let mut bg = match cell.bg {
            Color::Default => pal.bg,
            Color::Indexed(i) => {
                let i = if settings.ice_colors && flags.contains(Flags::BLINK) && i < 8 { i + 8 } else { i };
                pal.colors[i as usize]
            }
            Color::Rgb(r, g, b) => Rgb(r, g, b),
        };
        let reverse_defaults = term.modes.reverse_video;
        let mut inverse = flags.contains(Flags::INVERSE) != reverse_defaults;
        if selected {
            inverse = !inverse;
        }
        if inverse {
            std::mem::swap(&mut fg, &mut bg);
        }
        let mut fg32 = c32(fg);
        let bg32 = c32(bg);
        if flags.contains(Flags::DIM) {
            fg32 = fg32.lerp_to_gamma(bg32, 0.4);
        }
        let blink_hidden = flags.contains(Flags::BLINK) && !settings.ice_colors && !blink_on;
        let hidden = flags.contains(Flags::HIDDEN) || blink_hidden;
        Style {
            fg: fg32,
            bg: bg32,
            underline: if flags.contains(Flags::DOUBLE_UNDERLINE) {
                2
            } else {
                u8::from(flags.contains(Flags::UNDERLINE))
            },
            strike: flags.contains(Flags::STRIKE),
            italic: flags.contains(Flags::ITALIC),
            hidden,
        }
    }

    fn first_abs_line(term: &Term) -> i64 {
        term.lines_scrolled as i64 - term.scrollback_len() as i64
    }

    fn point_at(&self, term: &Term, rect: Rect, pos: egui::Pos2) -> Point {
        let col = ((pos.x - rect.left()) / self.cell.x).floor().clamp(0.0, (term.cols() - 1) as f32) as usize;
        let row = ((pos.y - rect.top()) / self.cell.y).floor().clamp(0.0, (term.rows() - 1) as f32) as usize;
        let top = term.scrollback_len() - self.scroll.min(term.scrollback_len());
        Point { line: Self::first_abs_line(term) + (top + row) as i64, col }
    }

    fn cell_at(&self, term: &Term, rect: Rect, pos: egui::Pos2) -> (usize, usize) {
        let col = ((pos.x - rect.left()) / self.cell.x).floor().clamp(0.0, (term.cols() - 1) as f32) as usize;
        let row = ((pos.y - rect.top()) / self.cell.y).floor().clamp(0.0, (term.rows() - 1) as f32) as usize;
        (col, row)
    }

    fn row_at(term: &Term, line: i64) -> Option<&crate::term::Row> {
        let idx = line - Self::first_abs_line(term);
        if idx < 0 {
            None
        } else {
            term.line(idx as usize)
        }
    }

    fn expand_selection(&self, term: &Term, sel: Selection) -> (Point, Point) {
        let (mut a, mut b) = sel.ordered();
        match sel.mode {
            SelectMode::Char => {}
            SelectMode::Line => {
                a.col = 0;
                b.col = term.cols() - 1;
            }
            SelectMode::Word => {
                if let Some(row) = Self::row_at(term, a.line) {
                    let cells = &row.cells;
                    if a.col < cells.len() && is_word_char(cells[a.col].ch) {
                        while a.col > 0 && is_word_char(cells[a.col - 1].ch) {
                            a.col -= 1;
                        }
                    }
                }
                if let Some(row) = Self::row_at(term, b.line) {
                    let cells = &row.cells;
                    if b.col < cells.len() && is_word_char(cells[b.col].ch) {
                        while b.col + 1 < cells.len() && is_word_char(cells[b.col + 1].ch) {
                            b.col += 1;
                        }
                    }
                }
            }
        }
        (a, b)
    }

    fn selected(&self, range: Option<(Point, Point)>, line: i64, col: usize) -> bool {
        match range {
            Some((a, b)) => {
                let p = Point { line, col };
                p >= a && p <= b
            }
            None => false,
        }
    }

    pub fn selection_text(&self, term: &Term) -> Option<String> {
        let sel = self.selection?;
        let (a, b) = self.expand_selection(term, sel);
        let mut out = String::new();
        let mut line = a.line;
        while line <= b.line {
            let Some(row) = Self::row_at(term, line) else {
                line += 1;
                continue;
            };
            let start = if line == a.line { a.col } else { 0 };
            let end = if line == b.line { b.col + 1 } else { row.cells.len() };
            let mut s: String = row.cells[start.min(row.cells.len())..end.min(row.cells.len())]
                .iter()
                .filter(|c| !c.flags.contains(Flags::WIDE_SPACER))
                .map(|c| c.ch)
                .collect();
            let wrapped = row.wrapped && line != b.line;
            if !wrapped {
                let t = s.trim_end().len();
                s.truncate(t);
            }
            out.push_str(&s);
            if line != b.line && !wrapped {
                out.push('\n');
            }
            line += 1;
        }
        Some(out)
    }

    pub fn select_all(&mut self, term: &Term) {
        let first = Self::first_abs_line(term);
        self.selection = Some(Selection {
            anchor: Point { line: first, col: 0 },
            head: Point { line: first + term.total_lines() as i64 - 1, col: term.cols() - 1 },
            mode: SelectMode::Char,
        });
    }

    /// Show the terminal in the remaining space of `ui`.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        emu: &mut Emulator,
        settings: &Settings,
        input_enabled: bool,
    ) -> ViewOutput {
        let mut out = ViewOutput::default();
        let font = FontId::monospace(settings.font_size);
        self.update_metrics(ui.ctx(), &font);
        let ppp = ui.ctx().pixels_per_point();

        let avail = ui.available_rect_before_wrap();
        let response = ui.allocate_rect(avail, Sense::click_and_drag());
        let id = response.id;
        if input_enabled && (response.clicked() || response.drag_started() || ui.memory(|m| m.focused().is_none())) {
            response.request_focus();
        }
        let focused = input_enabled && response.has_focus();
        if focused {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    id,
                    egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true },
                )
            });
        }
        self.has_focus = focused;

        // Fit the grid to the widget.
        let (cols, rows) = self.grid_for(avail.size());
        if cols != emu.term.cols() || rows != emu.term.rows() {
            emu.term.resize(cols, rows);
            out.resized = Some((
                cols as u16,
                rows as u16,
                (cols as f32 * self.cell.x * ppp) as u16,
                (rows as f32 * self.cell.y * ppp) as u16,
            ));
        }
        let grid_rect = Rect::from_min_size(avail.min, vec2(cols as f32 * self.cell.x, rows as f32 * self.cell.y));

        // Keep the viewport stable while scrolled back.
        let scrolled = emu.term.lines_scrolled;
        if self.scroll > 0 {
            self.scroll += scrolled.saturating_sub(self.last_lines_scrolled) as usize;
        }
        self.last_lines_scrolled = scrolled;
        self.scroll = self.scroll.min(emu.term.scrollback_len());

        if focused || response.hovered() {
            self.handle_input(ui, emu, settings, grid_rect, focused, &mut out);
        }

        self.paint(ui, &emu.term, settings, avail, grid_rect, &font);
        out
    }

    fn handle_input(
        &mut self,
        ui: &egui::Ui,
        emu: &mut Emulator,
        settings: &Settings,
        rect: Rect,
        focused: bool,
        out: &mut ViewOutput,
    ) {
        let events = ui.input(|i| i.events.clone());
        // Off macOS, egui turns Ctrl+C/X/V into Copy/Cut/Paste events, but so
        // are Ctrl+Insert / Shift+Delete / Shift+Insert on Windows. Only the
        // Ctrl+letter forms should reach the remote as control characters.
        let clipboard_key = ui.input(|i| i.key_down(Key::Insert) || i.key_down(Key::Delete));
        let key_opts =
            KeyOptions { backspace_sends_del: settings.backspace_sends_del, option_as_meta: settings.option_as_meta };
        let mut typed = false;
        for ev in events {
            match ev {
                Event::WindowFocused(f) => {
                    self.window_focused = f;
                    if emu.term.modes.focus_events {
                        out.send.extend_from_slice(if f { b"\x1b[I" } else { b"\x1b[O" });
                    }
                }
                Event::Key { key, pressed: true, modifiers, .. } if focused => {
                    // View scrolling shortcuts.
                    if modifiers.shift
                        && !modifiers.ctrl
                        && matches!(key, Key::PageUp | Key::PageDown | Key::Home | Key::End)
                    {
                        let page = emu.term.rows().saturating_sub(1).max(1);
                        match key {
                            Key::PageUp => self.scroll += page,
                            Key::PageDown => self.scroll = self.scroll.saturating_sub(page),
                            Key::Home => self.scroll = emu.term.scrollback_len(),
                            _ => self.scroll = 0,
                        }
                        self.scroll = self.scroll.min(emu.term.scrollback_len());
                        continue;
                    }
                    if !cfg!(target_os = "macos") && modifiers.ctrl && modifiers.shift {
                        match key {
                            Key::C => {
                                out.copy = self.selection_text(&emu.term);
                                continue;
                            }
                            Key::V => {
                                // Handled by the app via the clipboard; egui doesn't
                                // provide the text here.
                                continue;
                            }
                            _ => {}
                        }
                    }
                    if let Some(bytes) = input::key_bytes(key, &modifiers, &emu.term.modes, &key_opts) {
                        if modifiers.alt && settings.option_as_meta {
                            self.suppress_text = true;
                        }
                        out.send.extend_from_slice(&bytes);
                        typed = true;
                    }
                }
                Event::Text(t) if focused => {
                    if self.suppress_text {
                        self.suppress_text = false;
                        continue;
                    }
                    emu.encode_input(&t, &mut out.send);
                    typed = true;
                }
                Event::Ime(egui::ImeEvent::Commit(t)) if focused => {
                    emu.encode_input(&t, &mut out.send);
                    typed = true;
                }
                Event::Copy if focused => {
                    if cfg!(target_os = "macos") || clipboard_key {
                        out.copy = self.selection_text(&emu.term);
                    } else {
                        out.send.push(0x03);
                        typed = true;
                    }
                }
                Event::Cut if focused => {
                    if cfg!(target_os = "macos") || clipboard_key {
                        out.copy = self.selection_text(&emu.term);
                    } else {
                        out.send.push(0x18);
                        typed = true;
                    }
                }
                Event::Paste(text) if focused => {
                    if cfg!(target_os = "macos") || clipboard_key {
                        self.paste(emu, &text, &mut out.send);
                        typed = true;
                    } else {
                        out.send.push(0x16);
                        typed = true;
                    }
                }
                Event::MouseWheel { unit, delta, modifiers, .. } => {
                    let lines = match unit {
                        egui::MouseWheelUnit::Line => delta.y,
                        egui::MouseWheelUnit::Point => delta.y / self.cell.y,
                        egui::MouseWheelUnit::Page => delta.y * emu.term.rows() as f32,
                    };
                    self.wheel_accum += lines;
                    let whole = self.wheel_accum.trunc();
                    self.wheel_accum -= whole;
                    let n = whole.abs() as usize;
                    if n == 0 {
                        continue;
                    }
                    let up = whole > 0.0;
                    let pos = ui.input(|i| i.pointer.hover_pos()).unwrap_or(rect.min);
                    let (col, row) = self.cell_at(&emu.term, rect, pos);
                    if emu.term.modes.mouse != MouseMode::Off && !modifiers.shift {
                        for _ in 0..n.min(10) {
                            let a = if up { MouseAction::WheelUp } else { MouseAction::WheelDown };
                            if let Some(b) = input::mouse_report(&emu.term.modes, a, col, row, &modifiers) {
                                out.send.extend_from_slice(&b);
                            }
                        }
                    } else if emu.term.alt_screen() && emu.term.modes.alternate_scroll {
                        let seq: &[u8] = match (up, emu.term.modes.app_cursor) {
                            (true, true) => b"\x1bOA",
                            (true, false) => b"\x1b[A",
                            (false, true) => b"\x1bOB",
                            (false, false) => b"\x1b[B",
                        };
                        for _ in 0..n.min(10) {
                            out.send.extend_from_slice(seq);
                        }
                    } else if up {
                        self.scroll = (self.scroll + n).min(emu.term.scrollback_len());
                    } else {
                        self.scroll = self.scroll.saturating_sub(n);
                    }
                }
                Event::PointerButton { pos, button, pressed, modifiers } => {
                    if !rect.expand(2.0).contains(pos) && pressed {
                        continue;
                    }
                    let reporting = emu.term.modes.mouse != MouseMode::Off && !modifiers.shift;
                    let bcode = match button {
                        PointerButton::Primary => 0,
                        PointerButton::Middle => 1,
                        PointerButton::Secondary => 2,
                        _ => continue,
                    };
                    if reporting {
                        let (col, row) = self.cell_at(&emu.term, rect, pos);
                        let action = if pressed { MouseAction::Press(bcode) } else { MouseAction::Release(bcode) };
                        self.mouse_button_down = if pressed { Some(bcode) } else { None };
                        if let Some(b) = input::mouse_report(&emu.term.modes, action, col, row, &modifiers) {
                            out.send.extend_from_slice(&b);
                        }
                        continue;
                    }
                    if button == PointerButton::Primary {
                        if pressed {
                            let p = self.point_at(&emu.term, rect, pos);
                            let now = Instant::now();
                            let clicks = match self.last_click {
                                Some((t, lp, n))
                                    if now.duration_since(t) < Duration::from_millis(400) && lp.line == p.line =>
                                {
                                    (n % 3) + 1
                                }
                                _ => 1,
                            };
                            self.last_click = Some((now, p, clicks));
                            let mode = match clicks {
                                2 => SelectMode::Word,
                                3 => SelectMode::Line,
                                _ => SelectMode::Char,
                            };
                            if modifiers.shift && self.selection.is_some() {
                                if let Some(s) = self.selection.as_mut() {
                                    s.head = p;
                                }
                            } else {
                                self.selection = Some(Selection { anchor: p, head: p, mode });
                            }
                            self.dragging = true;
                        } else {
                            self.dragging = false;
                            if let Some(s) = self.selection {
                                if s.mode == SelectMode::Char && s.anchor == s.head {
                                    self.selection = None;
                                }
                            }
                        }
                    } else if button == PointerButton::Middle && pressed {
                        if let Some(text) = self.selection_text(&emu.term) {
                            self.paste(emu, &text, &mut out.send);
                        }
                    }
                }
                Event::PointerMoved(pos) => {
                    let reporting = emu.term.modes.mouse != MouseMode::Off && !ui.input(|i| i.modifiers.shift);
                    if reporting && rect.contains(pos) {
                        let cell = self.cell_at(&emu.term, rect, pos);
                        if Some(cell) != self.last_report_cell {
                            self.last_report_cell = Some(cell);
                            let b = self.mouse_button_down.unwrap_or(3);
                            let mods = ui.input(|i| i.modifiers);
                            if let Some(bytes) =
                                input::mouse_report(&emu.term.modes, MouseAction::Motion(b), cell.0, cell.1, &mods)
                            {
                                out.send.extend_from_slice(&bytes);
                            }
                        }
                    } else if self.dragging {
                        let p = self.point_at(&emu.term, rect, pos);
                        if let Some(s) = self.selection.as_mut() {
                            s.head = p;
                        }
                        // Auto-scroll when dragging past the edges.
                        if pos.y < rect.top() {
                            self.scroll = (self.scroll + 1).min(emu.term.scrollback_len());
                        } else if pos.y > rect.bottom() {
                            self.scroll = self.scroll.saturating_sub(1);
                        }
                    }
                }
                _ => {}
            }
        }
        if typed {
            self.scroll = 0;
            self.selection = None;
        }
    }

    pub fn paste(&mut self, emu: &Emulator, text: &str, out: &mut Vec<u8>) {
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        if emu.term.modes.bracketed_paste {
            out.extend_from_slice(b"\x1b[200~");
            // Drop ESC and the other control characters (as xterm does) so
            // the pasted text can't end the bracket early and inject keystrokes.
            emu.encode_input(&sanitize_bracketed_paste(&text), out);
            out.extend_from_slice(b"\x1b[201~");
        } else {
            emu.encode_input(&text, out);
        }
        self.scroll = 0;
    }

    fn paint(&mut self, ui: &egui::Ui, term: &Term, settings: &Settings, avail: Rect, rect: Rect, font: &FontId) {
        let painter = ui.painter_at(avail);
        let ppp = ui.ctx().pixels_per_point();
        let focused = self.has_focus;
        let pal = &term.palette;
        let default_bg = c32(if term.modes.reverse_video { pal.fg } else { pal.bg });
        painter.rect_filled(avail, 0.0, default_bg);

        let now = Instant::now();
        let elapsed = now.duration_since(self.epoch).as_millis();
        let blink_on = (elapsed / 500).is_multiple_of(2);
        let cursor_blink_on = (elapsed / 530).is_multiple_of(2);
        let mut needs_blink_repaint = false;

        let sel_range = self.selection.map(|s| self.expand_selection(term, s));
        let first_abs = Self::first_abs_line(term);
        let top = term.scrollback_len() - self.scroll.min(term.scrollback_len());
        let spacing = self.cell.x - self.advance;
        let text_dy = ((self.cell.y - self.row_height) / 2.0 * ppp).round() / ppp;
        let cw = self.cell.x;
        let ch = self.cell.y;

        for vy in 0..term.rows() {
            let Some(row) = term.line(top + vy) else { continue };
            let abs_line = first_abs + (top + vy) as i64;
            let y = rect.top() + vy as f32 * ch;
            let ncols = row.cells.len().min(term.cols());

            // Resolve styles once.
            let styles: Vec<Style> = (0..ncols)
                .map(|x| {
                    let cell = &row.cells[x];
                    if cell.flags.contains(Flags::BLINK) && !settings.ice_colors {
                        needs_blink_repaint = true;
                    }
                    self.resolve(term, cell, settings, self.selected(sel_range, abs_line, x), blink_on)
                })
                .collect();

            // Backgrounds, merged into runs.
            let mut x = 0;
            while x < ncols {
                let bg = styles[x].bg;
                let mut end = x + 1;
                while end < ncols && styles[end].bg == bg {
                    end += 1;
                }
                if bg != default_bg {
                    let r = Rect::from_min_max(
                        pos2(rect.left() + x as f32 * cw, y),
                        pos2(rect.left() + end as f32 * cw, y + ch),
                    );
                    painter.rect_filled(r, 0.0, bg);
                }
                x = end;
            }

            // Text: one layout job per row; cells we draw separately become spaces.
            let mut job = LayoutJob::default();
            job.wrap.max_width = f32::INFINITY;
            let mut run = String::new();
            let mut run_fmt: Option<TextFormat> = None;
            let mut separate: Vec<(usize, char, bool)> = Vec::new();
            let flush = |job: &mut LayoutJob, run: &mut String, fmt: &Option<TextFormat>| {
                if let Some(f) = fmt {
                    if !run.is_empty() {
                        job.append(run, 0.0, f.clone());
                        run.clear();
                    }
                }
            };
            let mut any_text = false;
            for (x, (cell, &st)) in row.cells.iter().zip(&styles).enumerate() {
                let mut c = cell.ch;
                if cell.flags.contains(Flags::WIDE_SPACER) || st.hidden {
                    c = ' ';
                } else if c != ' ' {
                    let wide = cell.flags.contains(Flags::WIDE);
                    let special = settings.builtin_box_drawing && boxdraw::is_special(c);
                    let simple = !wide && !special && (c.is_ascii() || self.has_glyph(ui.ctx(), font, c));
                    if !simple {
                        separate.push((x, c, wide));
                        c = ' ';
                    } else {
                        any_text = true;
                    }
                }
                let fmt = TextFormat {
                    font_id: font.clone(),
                    extra_letter_spacing: spacing,
                    color: if c == ' ' { Color32::TRANSPARENT } else { st.fg },
                    italics: st.italic,
                    underline: if st.underline > 0 { Stroke::new((1.0 / ppp).max(1.0), st.fg) } else { Stroke::NONE },
                    strikethrough: if st.strike { Stroke::new((1.0 / ppp).max(1.0), st.fg) } else { Stroke::NONE },
                    ..Default::default()
                };
                // Plain spaces with no decoration can share any run.
                let decor = st.underline > 0 || st.strike;
                if c == ' ' && !decor {
                    if let Some(f) = &run_fmt {
                        if f.underline == Stroke::NONE && f.strikethrough == Stroke::NONE {
                            run.push(' ');
                            continue;
                        }
                    }
                }
                if run_fmt.as_ref() != Some(&fmt) {
                    flush(&mut job, &mut run, &run_fmt);
                    run_fmt = Some(fmt);
                }
                run.push(c);
                if st.underline > 1 {
                    any_text = true;
                }
                if decor {
                    any_text = true;
                }
            }
            flush(&mut job, &mut run, &run_fmt);
            if any_text {
                let galley = ui.ctx().fonts_mut(|f| f.layout_job(job));
                painter.galley(pos2(rect.left(), y + text_dy), galley, Color32::WHITE);
                // Double underline: a second line under the first.
                for (x, st) in styles.iter().enumerate() {
                    if st.underline == 2 && !st.hidden {
                        let ly = (y + ch - 1.0 / ppp * 1.5).min(y + ch);
                        painter.line_segment(
                            [pos2(rect.left() + x as f32 * cw, ly), pos2(rect.left() + (x + 1) as f32 * cw, ly)],
                            Stroke::new(1.0 / ppp, st.fg),
                        );
                    }
                }
            }
            for (x, c, wide) in separate {
                let st = styles[x];
                let cell_rect = Rect::from_min_size(
                    pos2(rect.left() + x as f32 * cw, y),
                    vec2(if wide { cw * 2.0 } else { cw }, ch),
                );
                if settings.builtin_box_drawing && boxdraw::draw(&painter, cell_rect, c, st.fg, st.bg, ppp) {
                    continue;
                }
                self.draw_glyph(&painter, font, cell_rect, c, st.fg, st.italic);
            }
        }

        // Cursor.
        let cur = &term.cursor;
        let cursor_row = top_row_of_cursor(term, top);
        if term.modes.cursor_visible {
            if let Some(vy) = cursor_row {
                let x = cur.x.min(term.cols() - 1);
                let row = term.screen_row(cur.y);
                let wide = row.cells[x].flags.contains(Flags::WIDE);
                let w = if wide { cw * 2.0 } else { cw };
                let r =
                    Rect::from_min_size(pos2(rect.left() + x as f32 * cw, rect.top() + vy as f32 * ch), vec2(w, ch));
                let color = c32(pal.cursor);
                let show = !focused || !term.modes.cursor_blink || cursor_blink_on;
                if term.modes.cursor_blink && focused {
                    needs_blink_repaint = true;
                }
                if show {
                    if !focused {
                        painter.rect_stroke(r, 0.0, Stroke::new(1.0, color), StrokeKind::Inside);
                    } else {
                        match term.cursor_shape {
                            CursorShape::Block => {
                                painter.rect_filled(r, 0.0, color);
                                let cell = row.cells[x];
                                let st = self.resolve(term, &cell, settings, false, true);
                                if cell.ch != ' ' && !st.hidden {
                                    let fg = st.bg;
                                    if !(settings.builtin_box_drawing
                                        && boxdraw::draw(&painter, r, cell.ch, fg, color, ppp))
                                    {
                                        self.draw_glyph(&painter, font, r, cell.ch, fg, st.italic);
                                    }
                                }
                            }
                            CursorShape::Underline => {
                                let h = (ch / 8.0).max(2.0 / ppp);
                                painter.rect_filled(
                                    Rect::from_min_max(pos2(r.left(), r.bottom() - h), r.max),
                                    0.0,
                                    color,
                                );
                            }
                            CursorShape::Bar => {
                                let w = (cw / 6.0).max(2.0 / ppp);
                                painter.rect_filled(
                                    Rect::from_min_max(r.min, pos2(r.left() + w, r.bottom())),
                                    0.0,
                                    color,
                                );
                            }
                        }
                    }
                }
            }
        }

        // Visual bell.
        if term.bells != self.last_bells {
            self.last_bells = term.bells;
            self.bell_until = Some(now + Duration::from_millis(120));
        }
        if let Some(until) = self.bell_until {
            if now < until {
                painter.rect_filled(avail, 0.0, Color32::from_white_alpha(40));
                ui.ctx().request_repaint_after(until - now);
            } else {
                self.bell_until = None;
            }
        }

        // Scrollback indicator.
        if self.scroll > 0 {
            let text = format!("↑ {} / {}", self.scroll, term.scrollback_len());
            let galley = painter.layout_no_wrap(text, FontId::proportional(12.0), Color32::BLACK);
            let size = galley.size() + vec2(12.0, 6.0);
            let r = Rect::from_min_size(pos2(avail.right() - size.x - 8.0, avail.top() + 6.0), size);
            painter.rect_filled(r, 4.0, Color32::from_rgb(0xFF, 0xFF, 0x55));
            painter.galley(r.min + vec2(6.0, 3.0), galley, Color32::BLACK);
        }

        if needs_blink_repaint {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
    }

    fn has_glyph(&mut self, ctx: &egui::Context, font: &FontId, c: char) -> bool {
        if let Some(&ok) = self.glyph_ok.get(&c) {
            return ok;
        }
        // Only glyphs from the primary monospace face keep the grid aligned;
        // anything from a fallback font is drawn on its own.
        let ok = ctx.fonts_mut(|f| {
            let w = f.glyph_width(font, c);
            f.has_glyph(font, c) && (w - self.advance).abs() < 0.01
        });
        self.glyph_ok.insert(c, ok);
        ok
    }

    fn draw_glyph(&self, painter: &egui::Painter, font: &FontId, cell: Rect, c: char, color: Color32, italic: bool) {
        let ctx = painter.ctx();
        let mut job = LayoutJob::single_section(
            c.to_string(),
            TextFormat { font_id: font.clone(), color, italics: italic, ..Default::default() },
        );
        job.wrap.max_width = f32::INFINITY;
        let galley = ctx.fonts_mut(|f| f.layout_job(job));
        let size = galley.size();
        // Shrink glyphs that are too wide for their cell(s).
        let pos = pos2(cell.center().x - size.x / 2.0, cell.center().y - size.y / 2.0);
        if size.x > cell.width() * 1.05 {
            let scale = cell.width() / size.x;
            let small = FontId::new(font.size * scale, font.family.clone());
            let mut job = LayoutJob::single_section(
                c.to_string(),
                TextFormat { font_id: small, color, italics: italic, ..Default::default() },
            );
            job.wrap.max_width = f32::INFINITY;
            let galley = ctx.fonts_mut(|f| f.layout_job(job));
            let size = galley.size();
            painter.galley(pos2(cell.center().x - size.x / 2.0, cell.center().y - size.y / 2.0), galley, color);
        } else {
            painter.galley(pos, galley, color);
        }
    }
}

/// The visible row the cursor is on, if it's in view.
fn top_row_of_cursor(term: &Term, top: usize) -> Option<usize> {
    let cursor_abs = term.scrollback_len() + term.cursor.y;
    if cursor_abs >= top && cursor_abs < top + term.rows() {
        Some(cursor_abs - top)
    } else {
        None
    }
}

/// Remove control characters (C0 except tab and CR, DEL and C1) from text
/// pasted in bracketed-paste mode.
fn sanitize_bracketed_paste(text: &str) -> String {
    text.chars().filter(|&c| c == '\t' || c == '\r' || !c.is_control()).collect()
}

#[cfg(test)]
mod tests {
    use super::sanitize_bracketed_paste;

    #[test]
    fn bracketed_paste_cannot_reconstruct_end_marker() {
        let evil = "harmless\x1b[20\x1b[201~1~touch /tmp/pwned\r";
        let clean = sanitize_bracketed_paste(evil);
        assert!(!clean.contains('\x1b'));
        assert_eq!(clean, "harmless[20[201~1~touch /tmp/pwned\r");
        assert_eq!(sanitize_bracketed_paste("a\tb\u{9b}c\u{7f}"), "a\tbc");
    }
}
