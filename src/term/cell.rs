//! Grid cells and attributes.

/// A terminal color as stored in a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// Cell attribute flags (a tiny hand-rolled bitflags type).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Flags(pub u16);

impl Flags {
    pub const BOLD: Flags = Flags(1);
    pub const DIM: Flags = Flags(1 << 1);
    pub const ITALIC: Flags = Flags(1 << 2);
    pub const UNDERLINE: Flags = Flags(1 << 3);
    pub const BLINK: Flags = Flags(1 << 4);
    pub const INVERSE: Flags = Flags(1 << 5);
    pub const HIDDEN: Flags = Flags(1 << 6);
    pub const STRIKE: Flags = Flags(1 << 7);
    pub const DOUBLE_UNDERLINE: Flags = Flags(1 << 8);
    pub const OVERLINE: Flags = Flags(1 << 9);
    /// This cell holds a double-width character.
    pub const WIDE: Flags = Flags(1 << 10);
    /// This cell is the right half of a double-width character.
    pub const WIDE_SPACER: Flags = Flags(1 << 11);

    #[inline]
    pub fn contains(self, other: Flags) -> bool {
        self.0 & other.0 == other.0
    }
    #[inline]
    pub fn insert(&mut self, other: Flags) {
        self.0 |= other.0;
    }
    #[inline]
    pub fn remove(&mut self, other: Flags) {
        self.0 &= !other.0;
    }
}

/// The current drawing attributes ("pen").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Pen {
    pub fg: Color,
    pub bg: Color,
    pub flags: Flags,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub flags: Flags,
}

impl Default for Cell {
    fn default() -> Self {
        Cell { ch: ' ', fg: Color::Default, bg: Color::Default, flags: Flags::default() }
    }
}

impl Cell {
    /// A blank cell that keeps the background of `pen` (as erase operations do).
    #[inline]
    pub fn blank(pen: &Pen) -> Cell {
        Cell { ch: ' ', fg: pen.fg, bg: pen.bg, flags: Flags::default() }
    }
}

#[derive(Debug, Clone)]
pub struct Row {
    pub cells: Vec<Cell>,
    /// The line was soft-wrapped into the next one.
    pub wrapped: bool,
}

impl Row {
    pub fn new(cols: usize, pen: &Pen) -> Row {
        Row { cells: vec![Cell::blank(pen); cols], wrapped: false }
    }

    pub fn resize(&mut self, cols: usize) {
        self.cells.resize(cols, Cell::default());
        if let Some(last) = self.cells.last_mut() {
            if last.flags.contains(Flags::WIDE) {
                *last = Cell::default();
            }
        }
    }

    pub fn clear(&mut self, pen: &Pen) {
        self.cells.fill(Cell::blank(pen));
        self.wrapped = false;
    }

    /// The row's text with trailing blanks removed.
    #[cfg(test)]
    pub fn text(&self) -> String {
        let mut s: String = self.cells.iter().filter(|c| !c.flags.contains(Flags::WIDE_SPACER)).map(|c| c.ch).collect();
        let trimmed = s.trim_end().len();
        s.truncate(trimmed);
        s
    }
}
