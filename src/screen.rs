//! A terminal screen, maintained server-side.
//!
//! An interactive coding cli draws with cursor movement and erase sequences,
//! not by appending lines, so its pty output means nothing unless something
//! interprets it. That happens here: bytes in, a grid of cells out, which the
//! console gets as changed rows and Telegram gets as plain text.
//!
//! Pure on purpose — no processes, no sockets, no async — so it can be tested.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::{json, Value};

/// What a session starts at, until a console says what it's really showing.
pub const DEFAULT_COLS: u16 = 80;
pub const DEFAULT_ROWS: u16 = 24;

/// Lines kept after they scroll off the top. Same spirit as
/// `MAX_EVENTS_PER_SESSION`: enough for a build, not enough to eat the machine.
const MAX_SCROLLBACK: usize = 4000;

const TAB_WIDTH: usize = 8;

/// A ceiling, so a client with broken arithmetic can't allocate the machine.
const MAX_COLS: u16 = 500;
const MAX_ROWS: u16 = 300;

/// Shared between the pump that feeds it and everything that reads it.
pub type Shared = Arc<Mutex<Screen>>;

/// Takes a poisoned lock anyway: a panic in the parser shouldn't cost the
/// session its output for good.
pub fn lock(screen: &Shared) -> MutexGuard<'_, Screen> {
    screen.lock().unwrap_or_else(|e| e.into_inner())
}

/// Kept in the form the sequence gave it, so the console applies its own
/// palette to the indexed ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    #[default]
    Default,
    /// One of the 256 palette slots.
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl Color {
    /// null for "the console's default", a number for a palette slot,
    /// `#rrggbb` for an exact colour.
    fn json(self) -> Value {
        match self {
            Color::Default => Value::Null,
            Color::Indexed(i) => json!(i),
            Color::Rgb(r, g, b) => json!(format!("#{r:02x}{g:02x}{b:02x}")),
        }
    }
}

/// What a cell carries. Small on purpose: these are what coding tuis use to
/// mean something, and every extra one is another thing to get subtly wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pen {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub inverse: bool,
}

impl Pen {
    /// Erasing paints the current background but not the foreground or
    /// weight, which is what xterm does.
    fn erasing(&self) -> Pen {
        Pen { bg: self.bg, ..Pen::default() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cell {
    pub ch: char,
    pub pen: Pen,
    /// Right-hand half of a double-width char: no glyph of its own, it just
    /// keeps the columns after it lined up.
    pub tail: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Cell { ch: ' ', pen: Pen::default(), tail: false }
    }
}

impl Cell {
    fn blank(pen: Pen) -> Self {
        Cell { ch: ' ', pen, tail: false }
    }

    /// Indistinguishable from empty space, so a row's tail can be dropped.
    fn is_blank(&self) -> bool {
        !self.tail && self.ch == ' ' && self.pen == Pen::default()
    }
}

type Row = Vec<Cell>;

/// How wide a character is on a terminal. A real width table is a megabyte of
/// dependency; these are the ranges that turn up in agent output.
fn char_width(c: char) -> usize {
    let cp = c as u32;
    if matches!(cp,
        0x0300..=0x036F | 0x0483..=0x0489 | 0x0591..=0x05BD | 0x200B..=0x200F
            | 0x20D0..=0x20FF | 0xFE00..=0xFE0F | 0xFE20..=0xFE2F | 0xE0100..=0xE01EF
    ) {
        return 0;
    }
    if matches!(cp,
        0x1100..=0x115F | 0x2E80..=0x303E | 0x3041..=0x33FF | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF | 0xA000..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF
            | 0xFE30..=0xFE6F | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6
            | 0x1F300..=0x1F64F | 0x1F900..=0x1F9FF | 0x20000..=0x2FFFD | 0x30000..=0x3FFFD
    ) {
        return 2;
    }
    1
}

/// A parser bolted to a grid. Two types because `vte` borrows the parser and
/// the thing it feeds at once, and `feed` destructures to get both.
pub struct Screen {
    parser: vte::Parser,
    grid: Grid,
}

impl std::fmt::Debug for Screen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Screen({}x{} rev {})", self.grid.cols, self.grid.rows, self.grid.revision)
    }
}

impl Screen {
    pub fn new(cols: u16, rows: u16) -> Self {
        Screen { parser: vte::Parser::new(), grid: Grid::new(cols, rows) }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        let Screen { parser, grid } = self;
        grid.begin();
        parser.advance(grid, bytes);
        grid.commit();
    }

    /// Ignored if the size is unchanged or nonsense.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.grid.resize(cols, rows);
    }

    pub fn revision(&self) -> u64 {
        self.grid.revision
    }

    pub fn cols(&self) -> u16 {
        self.grid.cols as u16
    }

    pub fn rows(&self) -> u16 {
        self.grid.rows as u16
    }

    /// Every row, for a console that has just opened.
    pub fn snapshot(&self) -> Value {
        self.grid.render(None)
    }

    /// Only the rows changed since `since`. A `since` from the future — a
    /// console that outlived the screen — gets a full snapshot instead.
    pub fn diff(&self, since: u64) -> Value {
        if since > self.grid.revision {
            return self.grid.render(None);
        }
        self.grid.render(Some(since))
    }

    /// The snapshot-or-diff choice the http endpoint makes.
    pub fn view(&self, since: Option<u64>) -> Value {
        match since {
            Some(rev) => self.diff(rev),
            None => self.snapshot(),
        }
    }

    /// Scrollback then screen, as plain lines. `tail_text` is what the callers
    /// so far want; this is the whole thing.
    #[allow(dead_code)]
    pub fn to_plain_text(&self) -> String {
        self.grid.plain_lines().join("\n")
    }

    /// The last `lines` lines of `to_plain_text`. Walks back from the end
    /// rather than rendering all of scrollback to keep a handful of rows —
    /// the master polls this while a worker runs.
    pub fn tail_text(&self, lines: usize) -> String {
        self.grid.tail_lines(lines).join("\n")
    }

    /// Lines that scrolled off the top since the last call, so the line-based
    /// event log keeps working for pty sessions.
    pub fn take_lines(&mut self) -> Vec<String> {
        std::mem::take(&mut self.grid.emitted)
    }
}

/// The cell grid and the cursor over it. This is the `vte::Perform`.
struct Grid {
    cols: usize,
    rows: usize,
    cells: Vec<Row>,
    /// The primary buffer, parked while the alternate screen is up.
    parked: Option<Vec<Row>>,
    alt: bool,
    scrollback: VecDeque<Row>,
    /// Left the top of the screen, not reported yet.
    emitted: Vec<String>,
    cx: usize,
    cy: usize,
    pen: Pen,
    saved: Option<(usize, usize, Pen)>,
    /// The scroll region, inclusive on both ends.
    top: usize,
    bot: usize,
    autowrap: bool,
    /// The cursor is parked past the last column and the next glyph wraps.
    wrap_next: bool,
    visible: bool,
    revision: u64,
    /// When each row last changed. This is the whole diff.
    row_rev: Vec<u64>,
    /// Rows touched by the feed being parsed.
    touched: Vec<bool>,
    cursor_before: (usize, usize),
}

impl Grid {
    fn new(cols: u16, rows: u16) -> Self {
        let cols = cols.clamp(1, MAX_COLS) as usize;
        let rows = rows.clamp(1, MAX_ROWS) as usize;
        Grid {
            cols,
            rows,
            cells: vec![vec![Cell::default(); cols]; rows],
            parked: None,
            alt: false,
            scrollback: VecDeque::new(),
            emitted: Vec::new(),
            cx: 0,
            cy: 0,
            pen: Pen::default(),
            saved: None,
            top: 0,
            bot: rows - 1,
            autowrap: true,
            wrap_next: false,
            visible: true,
            revision: 0,
            row_rev: vec![0; rows],
            touched: vec![false; rows],
            cursor_before: (0, 0),
        }
    }

    // ------------------------------------------------------------ revisions

    fn begin(&mut self) {
        self.cursor_before = (self.cy, self.cx);
    }

    /// Stamp the rows this feed touched. A bare cursor move counts too — the
    /// console draws the cursor, so it has to hear about it.
    fn commit(&mut self) {
        let moved = self.cursor_before != (self.cy, self.cx);
        let any = self.touched.iter().any(|t| *t);
        if !moved && !any {
            return;
        }
        self.revision += 1;
        for (i, touched) in self.touched.iter_mut().enumerate() {
            if *touched {
                *touched = false;
                self.row_rev[i] = self.revision;
            }
        }
    }

    fn touch(&mut self, row: usize) {
        if let Some(flag) = self.touched.get_mut(row) {
            *flag = true;
        }
    }

    fn touch_all(&mut self) {
        self.touched.fill(true);
    }

    // ---------------------------------------------------------- the cursor

    fn clamp_cursor(&mut self) {
        self.cx = self.cx.min(self.cols - 1);
        self.cy = self.cy.min(self.rows - 1);
    }

    fn goto(&mut self, row: usize, col: usize) {
        self.cy = row.min(self.rows - 1);
        self.cx = col.min(self.cols - 1);
        self.wrap_next = false;
    }

    // --------------------------------------------------------- the buffers

    fn blank_row(&self) -> Row {
        vec![Cell::blank(self.pen.erasing()); self.cols]
    }

    /// Move the region up by `n`, blanking what comes in below. Only a region
    /// at the real top of a primary screen makes history: a tui scrolling a
    /// pane in the middle isn't, and the alt screen never is.
    fn scroll_up(&mut self, n: usize) {
        let n = n.min(self.bot - self.top + 1);
        let keep_history = !self.alt && self.top == 0;
        for _ in 0..n {
            let row = self.cells.remove(self.top);
            if keep_history {
                self.emitted.push(row_text(&row));
                self.scrollback.push_back(row);
                while self.scrollback.len() > MAX_SCROLLBACK {
                    self.scrollback.pop_front();
                }
            }
            let blank = self.blank_row();
            self.cells.insert(self.bot, blank);
        }
        for row in self.top..=self.bot {
            self.touch(row);
        }
    }

    /// Move the region down by `n`. Nothing comes back out of the scrollback:
    /// what scrolled off is history, not a buffer to rewind.
    fn scroll_down(&mut self, n: usize) {
        let n = n.min(self.bot - self.top + 1);
        for _ in 0..n {
            self.cells.remove(self.bot);
            let blank = self.blank_row();
            self.cells.insert(self.top, blank);
        }
        for row in self.top..=self.bot {
            self.touch(row);
        }
    }

    fn linefeed(&mut self) {
        self.wrap_next = false;
        if self.cy == self.bot {
            self.scroll_up(1);
        } else if self.cy + 1 < self.rows {
            self.cy += 1;
        }
    }

    fn reverse_linefeed(&mut self) {
        self.wrap_next = false;
        if self.cy == self.top {
            self.scroll_down(1);
        } else if self.cy > 0 {
            self.cy -= 1;
        }
    }

    /// Switch to or from the alternate screen (private mode 1049). The primary
    /// buffer is parked whole, so leaving a tui uncovers what was under it.
    fn set_alt(&mut self, on: bool) {
        if on == self.alt {
            return;
        }
        if on {
            self.saved = Some((self.cy, self.cx, self.pen));
            self.parked = Some(std::mem::replace(
                &mut self.cells,
                vec![vec![Cell::default(); self.cols]; self.rows],
            ));
            self.alt = true;
            self.top = 0;
            self.bot = self.rows - 1;
            self.goto(0, 0);
        } else {
            if let Some(parked) = self.parked.take() {
                self.cells = parked;
            }
            self.alt = false;
            self.top = 0;
            self.bot = self.rows - 1;
            if let Some((cy, cx, pen)) = self.saved.take() {
                self.pen = pen;
                self.goto(cy, cx);
            }
            self.clamp_cursor();
        }
        self.touch_all();
    }

    fn reset(&mut self) {
        self.set_alt(false);
        self.cells = vec![vec![Cell::default(); self.cols]; self.rows];
        self.pen = Pen::default();
        self.saved = None;
        self.top = 0;
        self.bot = self.rows - 1;
        self.autowrap = true;
        self.visible = true;
        self.goto(0, 0);
        self.touch_all();
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        let cols = cols.clamp(1, MAX_COLS) as usize;
        let rows = rows.clamp(1, MAX_ROWS) as usize;
        if cols == self.cols && rows == self.rows {
            return;
        }

        // width first, so the row work below sees rows of the right shape
        for row in self.cells.iter_mut().chain(self.parked.iter_mut().flatten()) {
            row.resize(cols, Cell::default());
        }
        self.cols = cols;

        while self.cells.len() > rows {
            // shrink from the top, so the cursor and newest output survive
            let row = self.cells.remove(0);
            if !self.alt {
                self.emitted.push(row_text(&row));
                self.scrollback.push_back(row);
                while self.scrollback.len() > MAX_SCROLLBACK {
                    self.scrollback.pop_front();
                }
            }
            self.cy = self.cy.saturating_sub(1);
        }
        while self.cells.len() < rows {
            self.cells.push(vec![Cell::default(); cols]);
        }
        if let Some(parked) = &mut self.parked {
            parked.resize(rows, vec![Cell::default(); cols]);
        }

        self.rows = rows;
        self.top = 0;
        self.bot = rows - 1;
        self.row_rev = vec![self.revision; rows];
        self.touched = vec![false; rows];
        self.clamp_cursor();
        self.wrap_next = false;

        // every row a console holds is now stale, so bump now rather than
        // waiting for the next byte from the agent
        self.touch_all();
        self.commit();
    }

    // ------------------------------------------------------------- erasing

    fn erase_in_row(&mut self, row: usize, from: usize, to: usize) {
        let pen = self.pen.erasing();
        let cols = self.cols;
        if let Some(line) = self.cells.get_mut(row) {
            for cell in &mut line[from..to.min(cols)] {
                *cell = Cell::blank(pen);
            }
        }
        self.touch(row);
    }

    // ------------------------------------------------------------- writing

    fn put(&mut self, c: char, width: usize) {
        if self.wrap_next && self.autowrap {
            self.cx = 0;
            self.linefeed();
        }
        self.wrap_next = false;

        // a screen narrower than the glyph has nowhere to put it: wrapping
        // would still leave the tail cell off the row
        if width > self.cols {
            return;
        }

        // a double-width glyph never straddles the edge
        if self.cx + width > self.cols {
            if !self.autowrap {
                return;
            }
            self.cx = 0;
            self.linefeed();
        }

        let (cy, cx, pen) = (self.cy, self.cx, self.pen);
        if let Some(row) = self.cells.get_mut(cy) {
            row[cx] = Cell { ch: c, pen, tail: false };
            if width == 2 {
                row[cx + 1] = Cell { ch: ' ', pen, tail: true };
            }
        }
        self.touch(cy);

        self.cx += width;
        if self.cx >= self.cols {
            self.cx = self.cols - 1;
            self.wrap_next = true;
        }
    }

    // ---------------------------------------------------------- rendering

    /// Rows as json. `None` means all of them.
    fn render(&self, since: Option<u64>) -> Value {
        let lines: Vec<Value> = (0..self.rows)
            .filter(|r| since.is_none_or(|rev| self.row_rev[*r] > rev))
            .map(|r| json!({ "row": r, "runs": runs(&self.cells[r]) }))
            .collect();

        json!({
            "revision": self.revision,
            "cols": self.cols,
            "rows": self.rows,
            "alt": self.alt,
            "cursor": { "row": self.cy, "col": self.cx, "visible": self.visible },
            "full": since.is_none(),
            "lines": lines,
        })
    }

    /// Scrollback and screen as text, minus the empty tail a mostly-blank
    /// screen is full of.
    /// The last `lines` non-trailing-blank rows, oldest first.
    fn tail_lines(&self, lines: usize) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if lines == 0 {
            return out;
        }

        let mut rows = self.scrollback.iter().chain(self.cells.iter()).rev();

        // the trailing blanks `plain_lines` drops are the rows a mostly-empty
        // screen ends with, so skip them before counting
        for row in rows.by_ref() {
            let text = row_text(row);
            if !text.is_empty() {
                out.push(text);
                break;
            }
        }
        while out.len() < lines {
            match rows.next() {
                Some(row) => out.push(row_text(row)),
                None => break,
            }
        }

        out.reverse();
        out
    }

    fn plain_lines(&self) -> Vec<String> {
        let mut out: Vec<String> =
            self.scrollback.iter().chain(self.cells.iter()).map(|r| row_text(r)).collect();
        while out.last().is_some_and(|l| l.is_empty()) {
            out.pop();
        }
        out
    }
}

/// A row as runs of identical attributes. Trailing blanks are dropped — most
/// of a terminal is empty, and the console pads rows out to `cols` anyway.
fn runs(row: &[Cell]) -> Vec<Value> {
    let end = row.iter().rposition(|c| !c.is_blank()).map(|i| i + 1).unwrap_or(0);
    let mut out: Vec<Value> = Vec::new();
    let mut text = String::new();
    let mut pen: Option<Pen> = None;

    for cell in &row[..end] {
        // no character of its own — the console gives the glyph before it
        // two columns
        if cell.tail {
            continue;
        }
        if pen != Some(cell.pen) {
            if let Some(p) = pen.take() {
                out.push(run_json(&text, p));
                text.clear();
            }
            pen = Some(cell.pen);
        }
        text.push(cell.ch);
    }
    if let Some(p) = pen {
        out.push(run_json(&text, p));
    }
    out
}

fn run_json(text: &str, pen: Pen) -> Value {
    json!({
        "text": text,
        "fg": pen.fg.json(),
        "bg": pen.bg.json(),
        "bold": pen.bold,
        "inverse": pen.inverse,
    })
}

fn row_text(row: &[Cell]) -> String {
    let mut out: String = row.iter().filter(|c| !c.tail).map(|c| c.ch).collect();
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

/// vte's own cap on how many parameters a csi can carry.
const MAX_PARAMS: usize = 32;

/// A csi's parameters, flattened so `38:5:1` and `38;5;1` read the same.
///
/// Borrowed and stack-held: this runs for every escape sequence a redrawing
/// tui emits, and the `Vec<Vec<u16>>` it used to build was a heap allocation
/// per parameter before dispatch had even looked at the action.
struct Args<'a> {
    slots: [&'a [u16]; MAX_PARAMS],
    len: usize,
}

impl<'a> Args<'a> {
    fn new(params: &'a vte::Params) -> Self {
        let mut slots = [&[][..]; MAX_PARAMS];
        let mut len = 0;
        for p in params.iter() {
            if len == MAX_PARAMS {
                break;
            }
            slots[len] = p;
            len += 1;
        }
        Args { slots, len }
    }
}

impl<'a> std::ops::Deref for Args<'a> {
    type Target = [&'a [u16]];

    fn deref(&self) -> &Self::Target {
        &self.slots[..self.len]
    }
}

/// One parameter; 0 and absent both mean the default, as they do everywhere
/// this is used.
fn arg(all: &[&[u16]], index: usize, default: usize) -> usize {
    match all.get(index).and_then(|p| p.first()).copied() {
        Some(0) | None => default,
        Some(v) => v as usize,
    }
}

impl vte::Perform for Grid {
    fn print(&mut self, c: char) {
        match char_width(c) {
            // a combining mark would otherwise eat a column and shift the row
            0 => {}
            width => self.put(c, width),
        }
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x08 => {
                // never wraps back past the left edge
                self.cx = self.cx.saturating_sub(1);
                self.wrap_next = false;
            }
            0x09 => {
                let next = (self.cx / TAB_WIDTH + 1) * TAB_WIDTH;
                self.cx = next.min(self.cols - 1);
                self.wrap_next = false;
            }
            0x0a..=0x0c => self.linefeed(),
            0x0d => {
                self.cx = 0;
                self.wrap_next = false;
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        // charset selection and friends: acting on the final byte anyway
        // would corrupt the grid
        if !intermediates.is_empty() {
            return;
        }
        match byte {
            b'D' => self.linefeed(),
            b'E' => {
                self.cx = 0;
                self.linefeed();
            }
            b'M' => self.reverse_linefeed(),
            b'7' => self.saved = Some((self.cy, self.cx, self.pen)),
            b'8' => {
                if let Some((cy, cx, pen)) = self.saved {
                    self.pen = pen;
                    self.goto(cy, cx);
                }
            }
            b'c' => self.reset(),
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &vte::Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        let all = Args::new(params);
        let private = intermediates.first() == Some(&b'?');

        // a different namespace entirely
        if private {
            match action {
                'h' | 'l' => {
                    let on = action == 'h';
                    for p in all.iter() {
                        match p.first().copied().unwrap_or(0) {
                            7 => self.autowrap = on,
                            25 => self.visible = on,
                            47 | 1047 | 1049 => self.set_alt(on),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            return;
        }
        if !intermediates.is_empty() {
            return;
        }

        match action {
            'A' => {
                let n = arg(&all, 0, 1);
                let floor = if self.cy >= self.top { self.top } else { 0 };
                self.cy = self.cy.saturating_sub(n).max(floor);
                self.wrap_next = false;
            }
            'B' | 'e' => {
                let n = arg(&all, 0, 1);
                let ceiling = if self.cy <= self.bot { self.bot } else { self.rows - 1 };
                self.cy = (self.cy + n).min(ceiling);
                self.wrap_next = false;
            }
            'C' | 'a' => {
                let n = arg(&all, 0, 1);
                self.cx = (self.cx + n).min(self.cols - 1);
                self.wrap_next = false;
            }
            'D' => {
                let n = arg(&all, 0, 1);
                self.cx = self.cx.saturating_sub(n);
                self.wrap_next = false;
            }
            'E' => {
                let n = arg(&all, 0, 1);
                self.cy = (self.cy + n).min(self.rows - 1);
                self.goto(self.cy, 0);
            }
            'F' => {
                let n = arg(&all, 0, 1);
                self.cy = self.cy.saturating_sub(n);
                self.goto(self.cy, 0);
            }
            'G' | '`' => {
                let col = arg(&all, 0, 1) - 1;
                self.goto(self.cy, col);
            }
            'd' => {
                let row = arg(&all, 0, 1) - 1;
                self.goto(row, self.cx);
            }
            'H' | 'f' => {
                let row = arg(&all, 0, 1) - 1;
                let col = arg(&all, 1, 1) - 1;
                self.goto(row, col);
            }
            'J' => {
                let (cy, cx, cols, rows) = (self.cy, self.cx, self.cols, self.rows);
                match arg(&all, 0, 0) {
                    0 => {
                        self.erase_in_row(cy, cx, cols);
                        for r in cy + 1..rows {
                            self.erase_in_row(r, 0, cols);
                        }
                    }
                    1 => {
                        for r in 0..cy {
                            self.erase_in_row(r, 0, cols);
                        }
                        self.erase_in_row(cy, 0, cx + 1);
                    }
                    2 => {
                        for r in 0..rows {
                            self.erase_in_row(r, 0, cols);
                        }
                    }
                    3 => {
                        self.scrollback.clear();
                    }
                    _ => {}
                }
            }
            'K' => {
                let (cy, cx, cols) = (self.cy, self.cx, self.cols);
                match arg(&all, 0, 0) {
                    0 => self.erase_in_row(cy, cx, cols),
                    1 => self.erase_in_row(cy, 0, cx + 1),
                    2 => self.erase_in_row(cy, 0, cols),
                    _ => {}
                }
            }
            'L' | 'M' => {
                // these act on the region below the cursor, which is a scroll
                // of a temporarily narrowed one
                if self.cy < self.top || self.cy > self.bot {
                    return;
                }
                let n = arg(&all, 0, 1);
                let was_top = self.top;
                self.top = self.cy;
                if action == 'L' {
                    self.scroll_down(n);
                } else {
                    self.scroll_up(n);
                }
                self.top = was_top;
            }
            '@' => {
                let n = arg(&all, 0, 1).min(self.cols - self.cx);
                let (cy, cx, cols, pen) = (self.cy, self.cx, self.cols, self.pen.erasing());
                if let Some(row) = self.cells.get_mut(cy) {
                    for _ in 0..n {
                        row.insert(cx, Cell::blank(pen));
                    }
                    row.truncate(cols);
                }
                self.touch(cy);
            }
            'P' => {
                let n = arg(&all, 0, 1).min(self.cols - self.cx);
                let (cy, cx, cols, pen) = (self.cy, self.cx, self.cols, self.pen.erasing());
                if let Some(row) = self.cells.get_mut(cy) {
                    for _ in 0..n {
                        row.remove(cx);
                    }
                    row.resize(cols, Cell::blank(pen));
                }
                self.touch(cy);
            }
            'X' => {
                let n = arg(&all, 0, 1);
                let (cy, cx) = (self.cy, self.cx);
                self.erase_in_row(cy, cx, cx + n);
            }
            'S' => {
                let n = arg(&all, 0, 1);
                self.scroll_up(n);
            }
            'T' => {
                let n = arg(&all, 0, 1);
                self.scroll_down(n);
            }
            'r' => {
                let top = arg(&all, 0, 1) - 1;
                let bot = arg(&all, 1, self.rows) - 1;
                // an inverted region would make every later scroll panic
                if top < bot && bot < self.rows {
                    self.top = top;
                    self.bot = bot;
                    self.goto(top, 0);
                }
            }
            's' => self.saved = Some((self.cy, self.cx, self.pen)),
            'u' => {
                if let Some((cy, cx, pen)) = self.saved {
                    self.pen = pen;
                    self.goto(cy, cx);
                }
            }
            'm' => self.sgr(&all),
            _ => {}
        }
    }
}

impl Grid {
    /// Select graphic rendition — colours and attributes.
    fn sgr(&mut self, all: &[&[u16]]) {
        if all.is_empty() {
            self.pen = Pen::default();
            return;
        }

        let mut i = 0;
        while i < all.len() {
            let param = &all[i];
            let code = param.first().copied().unwrap_or(0);
            match code {
                0 => self.pen = Pen::default(),
                1 => self.pen.bold = true,
                22 => self.pen.bold = false,
                7 => self.pen.inverse = true,
                27 => self.pen.inverse = false,
                30..=37 => self.pen.fg = Color::Indexed((code - 30) as u8),
                39 => self.pen.fg = Color::Default,
                40..=47 => self.pen.bg = Color::Indexed((code - 40) as u8),
                49 => self.pen.bg = Color::Default,
                90..=97 => self.pen.fg = Color::Indexed((code - 90 + 8) as u8),
                100..=107 => self.pen.bg = Color::Indexed((code - 100 + 8) as u8),
                38 | 48 => {
                    // written either 38:5:1 or 38;5;1
                    let (color, used) = if param.len() > 1 {
                        (extended(&param[1..]), 0)
                    } else {
                        // 2;r;g;b is the longest form `extended` reads
                        let mut buf = [0u16; 4];
                        let mut n = 0;
                        for p in &all[i + 1..] {
                            match (n < buf.len(), p.first()) {
                                (true, Some(v)) => {
                                    buf[n] = *v;
                                    n += 1;
                                }
                                _ => break,
                            }
                        }
                        let rest = &buf[..n];
                        let used = match rest.first() {
                            Some(5) => 2,
                            Some(2) => 4,
                            _ => 0,
                        };
                        (extended(rest), used)
                    };
                    if let Some(color) = color {
                        if code == 38 {
                            self.pen.fg = color;
                        } else {
                            self.pen.bg = color;
                        }
                    }
                    i += used;
                }
                _ => {}
            }
            i += 1;
        }
    }
}

/// The tail of a 38/48: `5;n` is a palette slot, `2;r;g;b` an exact colour.
fn extended(rest: &[u16]) -> Option<Color> {
    match rest.first().copied()? {
        5 => Some(Color::Indexed(*rest.get(1)? as u8)),
        2 => Some(Color::Rgb(
            *rest.get(1)? as u8,
            *rest.get(2)? as u8,
            *rest.get(3)? as u8,
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen() -> Screen {
        Screen::new(10, 4)
    }

    /// The visible grid as plain lines, row indexes intact.
    fn lines(s: &Screen) -> Vec<String> {
        s.grid.cells.iter().map(|r| row_text(r)).collect()
    }

    fn cursor(s: &Screen) -> (usize, usize) {
        (s.grid.cy, s.grid.cx)
    }

    #[test]
    fn printable_text_lands_where_the_cursor_is() {
        let mut s = screen();
        s.feed(b"hi");
        assert_eq!(lines(&s)[0], "hi");
        assert_eq!(cursor(&s), (0, 2));
    }

    #[test]
    fn cr_lf_and_backspace_do_what_they_say() {
        let mut s = screen();
        s.feed(b"abc\r\nde\x08f");
        assert_eq!(lines(&s)[0], "abc");
        assert_eq!(lines(&s)[1], "df");
    }

    #[test]
    fn a_tab_moves_to_the_next_stop() {
        let mut s = screen();
        s.feed(b"a\tb");
        assert_eq!(lines(&s)[0], "a       b");
        assert_eq!(cursor(&s).1, 9);
    }

    #[test]
    fn text_wraps_at_the_right_edge() {
        let mut s = screen();
        s.feed(b"0123456789ab");
        assert_eq!(lines(&s)[0], "0123456789");
        assert_eq!(lines(&s)[1], "ab");
    }

    #[test]
    fn cursor_movement_covers_the_usual_sequences() {
        let mut s = screen();
        s.feed(b"\x1b[2;3Hx"); // CUP, one-based
        assert_eq!(lines(&s)[1], "  x");

        s.feed(b"\x1b[Ay"); // CUU
        assert_eq!(lines(&s)[0], "   y");

        s.feed(b"\x1b[2Dz"); // CUB
        assert_eq!(lines(&s)[0], "  zy");

        s.feed(b"\x1b[3;1f\x1b[2Cw"); // HVP then CUF
        assert_eq!(lines(&s)[2], "  w");
    }

    #[test]
    fn erase_clears_what_it_is_asked_to_and_no_more() {
        let mut s = screen();
        s.feed(b"aaaa\r\nbbbb\r\ncccc");

        // EL 0 from the middle of the last row
        s.feed(b"\x1b[3;3H\x1b[K");
        assert_eq!(lines(&s)[2], "bb".replace('b', "c"));
        assert_eq!(lines(&s)[1], "bbbb");

        // ED 2 takes the lot
        s.feed(b"\x1b[2J");
        assert!(lines(&s).iter().all(|l| l.is_empty()));
    }

    #[test]
    fn el_1_erases_up_to_and_including_the_cursor() {
        let mut s = screen();
        s.feed(b"abcdef\x1b[1;3H\x1b[1K");
        assert_eq!(lines(&s)[0], "   def");
    }

    #[test]
    fn sgr_sets_colours_and_attributes_and_zero_clears_them() {
        let mut s = screen();
        s.feed(b"\x1b[1;31;44mx\x1b[0my");
        let row = &s.grid.cells[0];
        assert_eq!(row[0].pen, Pen { fg: Color::Indexed(1), bg: Color::Indexed(4), bold: true, inverse: false });
        assert_eq!(row[1].pen, Pen::default());
    }

    #[test]
    fn extended_colours_parse_both_ways_round() {
        let mut s = screen();
        s.feed(b"\x1b[38;5;196ma");
        assert_eq!(s.grid.cells[0][0].pen.fg, Color::Indexed(196));

        s.feed(b"\x1b[38;2;10;20;30mb");
        assert_eq!(s.grid.cells[0][1].pen.fg, Color::Rgb(10, 20, 30));

        // the colon form, which is what a lot of modern TUIs emit
        s.feed(b"\x1b[48:5:8mc");
        assert_eq!(s.grid.cells[0][2].pen.bg, Color::Indexed(8));
    }

    #[test]
    fn bright_colours_land_in_the_upper_half_of_the_palette() {
        let mut s = screen();
        s.feed(b"\x1b[93ma");
        assert_eq!(s.grid.cells[0][0].pen.fg, Color::Indexed(11));
    }

    #[test]
    fn scrolling_off_the_top_produces_lines_and_scrollback() {
        let mut s = screen();
        s.feed(b"one\r\ntwo\r\nthree\r\nfour\r\nfive");

        // four rows, five lines: the first one has gone to history
        assert_eq!(s.take_lines(), vec!["one".to_string()]);
        assert_eq!(lines(&s), vec!["two", "three", "four", "five"]);
        assert!(s.to_plain_text().starts_with("one\ntwo"));
        // and a second call doesn't hand the same line out twice
        assert!(s.take_lines().is_empty());
    }

    #[test]
    fn a_scroll_region_scrolls_only_itself() {
        let mut s = screen();
        s.feed(b"a\r\nb\r\nc\r\nd");
        // rows 2..3 only (one-based), cursor lands at the region top
        s.feed(b"\x1b[2;3r\x1b[3;1Hx\r\n\ny");

        assert_eq!(lines(&s)[0], "a", "the row above the region moved");
        assert_eq!(lines(&s)[3], "d", "the row below the region moved");
        // nothing left the screen, so nothing became history
        assert!(s.take_lines().is_empty());
    }

    #[test]
    fn an_inverted_scroll_region_is_ignored_rather_than_taken() {
        let mut s = screen();
        s.feed(b"\x1b[4;2r");
        assert_eq!((s.grid.top, s.grid.bot), (0, 3));
        // and the screen still works afterwards
        s.feed(b"ok");
        assert_eq!(lines(&s)[0], "ok");
    }

    #[test]
    fn the_alternate_screen_is_a_separate_buffer() {
        let mut s = screen();
        s.feed(b"shell output");

        s.feed(b"\x1b[?1049h");
        assert!(s.grid.alt);
        assert!(lines(&s).iter().all(|l| l.is_empty()), "the alt screen starts blank");
        s.feed(b"tui");
        assert_eq!(lines(&s)[0], "tui");

        s.feed(b"\x1b[?1049l");
        assert!(!s.grid.alt);
        assert_eq!(lines(&s)[0], "shell outp");
    }

    #[test]
    fn the_alternate_screen_never_writes_history() {
        let mut s = screen();
        s.feed(b"\x1b[?1049h");
        s.feed(b"1\r\n2\r\n3\r\n4\r\n5\r\n6");
        assert!(s.take_lines().is_empty());
        assert!(s.grid.scrollback.is_empty());
    }

    #[test]
    fn save_and_restore_bring_the_cursor_and_pen_back() {
        let mut s = screen();
        s.feed(b"\x1b[2;2H\x1b[31m\x1b7");
        s.feed(b"\x1b[4;4H\x1b[0mx");
        s.feed(b"\x1b8y");

        assert_eq!(cursor(&s), (1, 2));
        assert_eq!(s.grid.cells[1][1].pen.fg, Color::Indexed(1));
    }

    #[test]
    fn insert_and_delete_shift_the_row_without_changing_its_width() {
        let mut s = screen();
        s.feed(b"abcdef\x1b[1;2H\x1b[2@");
        assert_eq!(lines(&s)[0], "a  bcdef");
        assert_eq!(s.grid.cells[0].len(), 10);

        s.feed(b"\x1b[1;1H\x1b[3P");
        assert_eq!(lines(&s)[0], "bcdef");
        assert_eq!(s.grid.cells[0].len(), 10);
    }

    #[test]
    fn insert_and_delete_line_move_the_rows_below() {
        let mut s = screen();
        s.feed(b"a\r\nb\r\nc\r\nd");
        s.feed(b"\x1b[2;1H\x1b[L");
        assert_eq!(lines(&s), vec!["a", "", "b", "c"]);

        s.feed(b"\x1b[2;1H\x1b[M");
        assert_eq!(lines(&s), vec!["a", "b", "c", ""]);
    }

    #[test]
    fn a_wide_character_takes_two_columns() {
        let mut s = screen();
        s.feed("漢字x".as_bytes());
        assert_eq!(cursor(&s).1, 5);
        assert_eq!(lines(&s)[0], "漢字x");
        assert!(s.grid.cells[0][1].tail);
    }

    #[test]
    fn a_wide_character_on_a_one_column_screen_is_dropped_not_a_panic() {
        // the console can ask for a single column, and an agent can print cjk
        // into it — writing the tail cell used to index off the end of the row
        let mut s = Screen::new(1, 4);
        s.feed("漢a".as_bytes());
        assert_eq!(lines(&s)[0], "a");
    }

    #[test]
    fn combining_marks_do_not_consume_a_column() {
        let mut s = screen();
        s.feed("e\u{0301}x".as_bytes());
        assert_eq!(cursor(&s).1, 2);
        assert_eq!(lines(&s)[0], "ex");
    }

    #[test]
    fn unknown_sequences_are_ignored_rather_than_corrupting_the_grid() {
        let mut s = screen();
        s.feed(b"ab");
        // a device status report, an OSC title, a DCS, and a mode we don't do
        s.feed(b"\x1b[6n\x1b]0;a title\x07\x1bP+q544e\x1b\\\x1b[?2004h");
        s.feed(b"cd");
        assert_eq!(lines(&s)[0], "abcd");
    }

    #[test]
    fn a_sequence_split_across_two_feeds_still_parses() {
        let mut s = screen();
        s.feed(b"\x1b[2;");
        s.feed(b"3Hx");
        assert_eq!(lines(&s)[1], "  x");
    }

    #[test]
    fn the_revision_only_moves_when_something_changed() {
        let mut s = screen();
        let before = s.revision();
        s.feed(b"");
        assert_eq!(s.revision(), before);

        s.feed(b"x");
        assert!(s.revision() > before);
    }

    #[test]
    fn a_diff_carries_only_the_rows_that_changed() {
        let mut s = screen();
        s.feed(b"one\r\ntwo");
        let rev = s.revision();

        s.feed(b"\x1b[4;1Hfour");
        let diff = s.diff(rev);

        assert_eq!(diff["full"], false);
        let rows: Vec<u64> = diff["lines"].as_array().unwrap()
            .iter().map(|l| l["row"].as_u64().unwrap()).collect();
        assert_eq!(rows, vec![3]);
        assert_eq!(diff["revision"], s.revision());
    }

    #[test]
    fn a_diff_from_the_future_falls_back_to_a_full_snapshot() {
        let mut s = screen();
        s.feed(b"hello");
        let diff = s.diff(s.revision() + 100);
        assert_eq!(diff["full"], true);
        assert_eq!(diff["lines"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn a_snapshot_says_everything_the_console_needs() {
        let mut s = screen();
        s.feed(b"\x1b[31mred\x1b[0m plain");
        let snap = s.snapshot();

        assert_eq!(snap["cols"], 10);
        assert_eq!(snap["rows"], 4);
        assert_eq!(snap["alt"], false);
        assert_eq!(snap["full"], true);
        assert_eq!(snap["cursor"]["row"], 0);
        assert_eq!(snap["cursor"]["visible"], true);

        let runs = snap["lines"][0]["runs"].as_array().unwrap();
        assert_eq!(runs[0]["text"], "red");
        assert_eq!(runs[0]["fg"], 1);
        assert_eq!(runs[1]["fg"], Value::Null);
        // the row is ten wide but only nine are used, and blanks aren't shipped
        assert_eq!(runs.iter().map(|r| r["text"].as_str().unwrap().len()).sum::<usize>(), 9);
    }

    #[test]
    fn an_rgb_colour_reaches_the_console_as_hex() {
        let mut s = screen();
        s.feed(b"\x1b[38;2;255;0;128mx");
        let snap = s.snapshot();
        assert_eq!(snap["lines"][0]["runs"][0]["fg"], "#ff0080");
    }

    #[test]
    fn hiding_the_cursor_shows_up_in_the_snapshot() {
        let mut s = screen();
        s.feed(b"\x1b[?25l");
        assert_eq!(s.snapshot()["cursor"]["visible"], false);
        s.feed(b"\x1b[?25h");
        assert_eq!(s.snapshot()["cursor"]["visible"], true);
    }

    #[test]
    fn resizing_keeps_the_bottom_of_the_screen_and_bumps_the_revision() {
        let mut s = Screen::new(10, 4);
        s.feed(b"one\r\ntwo\r\nthree\r\nfour");
        let rev = s.revision();

        s.resize(10, 2);
        assert_eq!(s.rows(), 2);
        assert!(s.revision() > rev);
        assert_eq!(lines(&s), vec!["three", "four"]);
        // what fell off the top is history, not lost
        assert!(s.to_plain_text().starts_with("one\ntwo"));

        s.resize(6, 4);
        assert_eq!((s.cols(), s.rows()), (6, 4));
        assert_eq!(lines(&s)[0], "three");
    }

    #[test]
    fn resizing_to_the_same_size_is_a_no_op() {
        let mut s = screen();
        s.feed(b"x");
        let rev = s.revision();
        s.resize(10, 4);
        assert_eq!(s.revision(), rev);
    }

    #[test]
    fn a_silly_size_is_clamped_rather_than_panicking() {
        let mut s = Screen::new(0, 0);
        assert_eq!((s.cols(), s.rows()), (1, 1));
        s.resize(u16::MAX, u16::MAX);
        assert_eq!((s.cols(), s.rows()), (MAX_COLS, MAX_ROWS));
        s.feed(b"still fine");
    }

    #[test]
    fn plain_text_drops_the_empty_tail_but_keeps_the_middle() {
        let mut s = screen();
        s.feed(b"a\r\n\r\nb");
        assert_eq!(s.to_plain_text(), "a\n\nb");
        assert_eq!(s.tail_text(2), "\nb");
    }

    #[test]
    fn the_tail_is_the_end_of_the_plain_text_however_much_is_asked_for() {
        let mut s = screen();
        s.feed(b"one\r\ntwo\r\nthree");
        // the whole thing, the end of it, and none of it
        assert_eq!(s.tail_text(99), s.to_plain_text());
        assert_eq!(s.tail_text(2), "two\nthree");
        assert_eq!(s.tail_text(0), "");

        // and it still starts counting past the blank rows a short screen ends
        // with, the way plain_lines does
        let mut short = Screen::new(10, 6);
        short.feed(b"a\r\nb");
        assert_eq!(short.tail_text(1), "b");
    }

    #[test]
    fn a_full_reset_puts_everything_back() {
        let mut s = screen();
        s.feed(b"\x1b[?1049h\x1b[31mmess\x1b[2;5r");
        s.feed(b"\x1bc");
        assert!(!s.grid.alt);
        assert_eq!(cursor(&s), (0, 0));
        assert_eq!(s.grid.pen, Pen::default());
        assert!(lines(&s).iter().all(|l| l.is_empty()));
    }

    // the shape a coding tui takes: alt screen, hidden cursor, redraws in
    // place. it must not scroll history or drift.
    #[test]
    fn a_realistic_tui_redraw_stays_on_the_grid() {
        let mut s = Screen::new(20, 6);
        s.feed(b"\x1b[?1049h\x1b[?25l\x1b[2J\x1b[H");
        for frame in 0..30 {
            s.feed(format!("\x1b[H\x1b[2J\x1b[1;1Hframe {frame}\x1b[3;1H> prompt").as_bytes());
        }
        assert_eq!(lines(&s)[0], "frame 29");
        assert_eq!(lines(&s)[2], "> prompt");
        assert!(s.take_lines().is_empty());
        assert_eq!(s.rows(), 6);
    }
}
