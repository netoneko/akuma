//! Neko Text Editor
//!
//! A nano-like terminal text editor with pluggable I/O and filesystem.
//! Designed for `no_std` environments (uses `alloc`).

#![no_std]
#![allow(async_fn_in_trait, clippy::future_not_send)]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use embedded_io_async::{Read, Write};

// ============================================================================
// Filesystem Abstraction
// ============================================================================

/// Trait for filesystem operations needed by the editor.
///
/// Implement this to provide file loading and saving in your environment.
pub trait EditorFs {
    async fn read_to_string(&self, path: &str) -> Result<String, ()>;
    async fn write_file(&self, path: &str, data: &[u8]) -> Result<(), ()>;
}

// ============================================================================
// ANSI Escape Sequences
// ============================================================================

const CLEAR_SCREEN: &[u8] = b"\x1b[2J";
const CURSOR_HOME: &[u8] = b"\x1b[H";
const CURSOR_HIDE: &[u8] = b"\x1b[?25l";
const CURSOR_SHOW: &[u8] = b"\x1b[?25h";
const REVERSE_VIDEO: &[u8] = b"\x1b[7m";
const RESET_ATTRS: &[u8] = b"\x1b[0m";
const CLEAR_EOL: &[u8] = b"\x1b[K";

const RESIZE_SIGNAL_BYTE: u8 = 0x00;

// ============================================================================
// Editor Configuration
// ============================================================================

const LINE_NUM_WIDTH: usize = 6;

/// Terminal dimensions passed to the editor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TermSize {
    pub width: usize,
    pub height: usize,
}

impl TermSize {
    #[must_use]
    pub const fn new(width: u32, height: u32) -> Self {
        Self {
            width: width as usize,
            height: height as usize,
        }
    }

    const fn content_rows(&self) -> usize {
        if self.height > 4 { self.height - 4 } else { 1 }
    }
}

/// Trait for streams that can provide terminal size information.
pub trait TermSizeProvider {
    fn get_term_size(&self) -> TermSize;
}

// ============================================================================
// Editor Buffer
// ============================================================================

/// Manages the document content and cursor state.
pub struct EditorBuffer {
    lines: Vec<String>,
    cursor_row: usize,
    cursor_col: usize,
    scroll_offset: usize,
    modified: bool,
    filepath: String,
    term_size: TermSize,
}

impl EditorBuffer {
    #[must_use]
    pub fn new(filepath: &str, term_size: TermSize) -> Self {
        Self {
            lines: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
            scroll_offset: 0,
            modified: false,
            filepath: String::from(filepath),
            term_size,
        }
    }

    pub async fn load<F: EditorFs>(&mut self, fs: &F) -> Result<usize, &'static str> {
        if self.filepath.is_empty() {
            return Ok(0);
        }

        if let Ok(content) = fs.read_to_string(&self.filepath).await {
            self.lines = content.lines().map(String::from).collect();
            let line_count = self.lines.len();
            if self.lines.is_empty() {
                self.lines.push(String::new());
            }
            self.cursor_row = 0;
            self.cursor_col = 0;
            self.scroll_offset = 0;
            self.modified = false;
            Ok(line_count)
        } else {
            self.lines = vec![String::new()];
            self.modified = false;
            Ok(0)
        }
    }

    pub async fn save<F: EditorFs>(&mut self, fs: &F) -> Result<usize, &'static str> {
        if self.filepath.is_empty() {
            return Err("No filename specified");
        }

        let content = self.lines.join("\n");
        let bytes = content.as_bytes();

        if fs.write_file(&self.filepath, bytes).await.is_ok() {
            self.modified = false;
            Ok(bytes.len())
        } else {
            Err("Failed to write file")
        }
    }

    fn current_line(&self) -> &String {
        &self.lines[self.cursor_row]
    }

    pub fn insert_char(&mut self, c: char) {
        let row = self.cursor_row;
        let line_len = self.lines[row].len();
        if self.cursor_col > line_len {
            self.cursor_col = line_len;
        }
        let col = self.cursor_col;
        self.lines[row].insert(col, c);
        self.cursor_col += 1;
        self.modified = true;
    }

    pub fn delete_char_before(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
            let row = self.cursor_row;
            let col = self.cursor_col;
            self.lines[row].remove(col);
            self.modified = true;
        } else if self.cursor_row > 0 {
            let current = self.lines.remove(self.cursor_row);
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].len();
            self.lines[self.cursor_row].push_str(&current);
            self.modified = true;
            self.ensure_cursor_visible();
        }
    }

    pub fn delete_char_at(&mut self) {
        let line_len = self.current_line().len();
        if self.cursor_col < line_len {
            let row = self.cursor_row;
            let col = self.cursor_col;
            self.lines[row].remove(col);
            self.modified = true;
        } else if self.cursor_row < self.lines.len() - 1 {
            let next = self.lines.remove(self.cursor_row + 1);
            self.lines[self.cursor_row].push_str(&next);
            self.modified = true;
        }
    }

    pub fn insert_newline(&mut self) {
        let row = self.cursor_row;
        let col = self.cursor_col;
        let remainder = self.lines[row].split_off(col);
        self.cursor_row += 1;
        self.lines.insert(self.cursor_row, remainder);
        self.cursor_col = 0;
        self.modified = true;
        self.ensure_cursor_visible();
    }

    pub fn move_up(&mut self) {
        if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.clamp_cursor_col();
            self.ensure_cursor_visible();
        }
    }

    pub fn move_down(&mut self) {
        if self.cursor_row < self.lines.len() - 1 {
            self.cursor_row += 1;
            self.clamp_cursor_col();
            self.ensure_cursor_visible();
        }
    }

    pub fn move_left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].len();
            self.ensure_cursor_visible();
        }
    }

    pub fn move_right(&mut self) {
        let line_len = self.current_line().len();
        if self.cursor_col < line_len {
            self.cursor_col += 1;
        } else if self.cursor_row < self.lines.len() - 1 {
            self.cursor_row += 1;
            self.cursor_col = 0;
            self.ensure_cursor_visible();
        }
    }

    pub const fn move_home(&mut self) {
        self.cursor_col = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor_col = self.current_line().len();
    }

    fn clamp_cursor_col(&mut self) {
        let line_len = self.current_line().len();
        if self.cursor_col > line_len {
            self.cursor_col = line_len;
        }
    }

    const fn ensure_cursor_visible(&mut self) {
        let content_rows = self.term_size.content_rows();
        if self.cursor_row < self.scroll_offset {
            self.scroll_offset = self.cursor_row;
        } else if self.cursor_row >= self.scroll_offset + content_rows {
            self.scroll_offset = self.cursor_row - content_rows + 1;
        }
    }

    fn visible_lines(&self) -> impl Iterator<Item = (usize, &String)> + '_ {
        let content_rows = self.term_size.content_rows();
        self.lines
            .iter()
            .enumerate()
            .skip(self.scroll_offset)
            .take(content_rows)
    }

    const fn screen_cursor(&self) -> (usize, usize) {
        let row = self.cursor_row - self.scroll_offset + 3;
        let col = LINE_NUM_WIDTH + self.cursor_col + 1;
        (row, col)
    }

    #[cfg(test)]
    const fn cursor(&self) -> (usize, usize) {
        (self.cursor_row, self.cursor_col)
    }

    #[cfg(test)]
    fn line_count(&self) -> usize {
        self.lines.len()
    }

    #[cfg(test)]
    fn line_text(&self, row: usize) -> &str {
        &self.lines[row]
    }

    #[cfg(test)]
    const fn is_modified(&self) -> bool {
        self.modified
    }
}

// ============================================================================
// Editor State
// ============================================================================

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EditorMode {
    Normal,
    ConfirmQuit,
    Message,
}

/// Main editor state.
pub struct Editor {
    buffer: EditorBuffer,
    mode: EditorMode,
    message: String,
    running: bool,
    term_size: TermSize,
}

impl Editor {
    #[must_use]
    pub fn new(filepath: &str, term_size: TermSize) -> Self {
        Self {
            buffer: EditorBuffer::new(filepath, term_size),
            mode: EditorMode::Normal,
            message: String::new(),
            running: true,
            term_size,
        }
    }

    pub async fn load<F: EditorFs>(&mut self, fs: &F) -> Result<usize, &'static str> {
        let lines = self.buffer.load(fs).await?;
        if lines > 0 {
            self.set_message(&format!("Loaded {lines} lines"));
        } else {
            self.set_message("New file");
        }
        Ok(lines)
    }

    pub const fn update_term_size(&mut self, new_size: TermSize) {
        self.term_size = new_size;
        self.buffer.term_size = new_size;
        self.buffer.ensure_cursor_visible();
    }

    fn set_message(&mut self, msg: &str) {
        self.message = String::from(msg);
        self.mode = EditorMode::Message;
    }

    fn clear_message(&mut self) {
        self.message.clear();
        self.mode = EditorMode::Normal;
    }
}

// ============================================================================
// Input Parser
// ============================================================================

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EscapeState {
    Normal,
    Escape,
    Bracket,
    Extended,
}

/// Parsed input event from terminal byte stream.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputEvent {
    Char(char),
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Delete,
    Backspace,
    Enter,
    CtrlC,
    CtrlO,
    CtrlX,
    CtrlA,
    CtrlE,
    Resize,
    None,
}

/// Stateful parser that converts raw terminal bytes into `InputEvent`s.
pub struct InputParser {
    state: EscapeState,
    extended_char: u8,
}

impl InputParser {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: EscapeState::Normal,
            extended_char: 0,
        }
    }

    /// Feed a single byte and return the resulting event.
    pub fn feed(&mut self, byte: u8) -> InputEvent {
        if byte == RESIZE_SIGNAL_BYTE {
            return InputEvent::Resize;
        }

        match self.state {
            EscapeState::Normal => match byte {
                0x1b => {
                    self.state = EscapeState::Escape;
                    InputEvent::None
                }
                0x01 => InputEvent::CtrlA,
                0x03 => InputEvent::CtrlC,
                0x05 => InputEvent::CtrlE,
                0x0F => InputEvent::CtrlO,
                0x18 => InputEvent::CtrlX,
                0x0D | 0x0A => InputEvent::Enter,
                0x7F | 0x08 => InputEvent::Backspace,
                c if (0x20..0x7F).contains(&c) => InputEvent::Char(c as char),
                _ => InputEvent::None,
            },
            EscapeState::Escape => {
                if byte == b'[' {
                    self.state = EscapeState::Bracket;
                } else {
                    self.state = EscapeState::Normal;
                }
                InputEvent::None
            }
            EscapeState::Bracket => {
                self.state = EscapeState::Normal;
                match byte {
                    b'A' => InputEvent::Up,
                    b'B' => InputEvent::Down,
                    b'C' => InputEvent::Right,
                    b'D' => InputEvent::Left,
                    b'H' => InputEvent::Home,
                    b'F' => InputEvent::End,
                    b'3' | b'1' | b'4' | b'7' | b'8' => {
                        self.extended_char = byte;
                        self.state = EscapeState::Extended;
                        InputEvent::None
                    }
                    _ => InputEvent::None,
                }
            }
            EscapeState::Extended => {
                self.state = EscapeState::Normal;
                if byte == b'~' {
                    match self.extended_char {
                        b'3' => InputEvent::Delete,
                        b'1' | b'7' => InputEvent::Home,
                        b'4' | b'8' => InputEvent::End,
                        _ => InputEvent::None,
                    }
                } else {
                    InputEvent::None
                }
            }
        }
    }
}

impl Default for InputParser {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Screen Rendering
// ============================================================================

fn separator_line(buf: &mut Vec<u8>, width: usize) {
    buf.extend(core::iter::repeat_n(b'-', width));
    buf.extend_from_slice(b"\r\n");
}

/// Build the screen output into a byte buffer.
#[must_use]
pub fn build_screen_buffer(editor: &Editor) -> Vec<u8> {
    let mut buf = Vec::with_capacity(8192);
    let term_width = editor.term_size.width;
    let content_rows = editor.term_size.content_rows();

    buf.extend_from_slice(RESET_ATTRS);
    buf.extend_from_slice(CURSOR_HIDE);
    buf.extend_from_slice(CURSOR_HOME);
    buf.extend_from_slice(CLEAR_SCREEN);
    buf.extend_from_slice(CURSOR_HOME);

    buf.extend_from_slice(REVERSE_VIDEO);
    let title = if editor.buffer.filepath.is_empty() {
        String::from("  neko - [New File]")
    } else if editor.buffer.modified {
        format!("  neko - {} [Modified]", editor.buffer.filepath)
    } else {
        format!("  neko - {}", editor.buffer.filepath)
    };
    let padded_title = format!("{title:term_width$}");
    buf.extend_from_slice(padded_title.as_bytes());
    buf.extend_from_slice(RESET_ATTRS);
    buf.extend_from_slice(b"\r\n");

    separator_line(&mut buf, term_width);

    let mut row_count = 0;
    for (line_num, line) in editor.buffer.visible_lines() {
        let num = line_num + 1;
        let line_num_str = format!("{num:>4}| ");
        buf.extend_from_slice(line_num_str.as_bytes());

        let available_width = term_width.saturating_sub(LINE_NUM_WIDTH);
        let display_line: String = line.chars().take(available_width).collect();
        buf.extend_from_slice(display_line.as_bytes());
        buf.extend_from_slice(CLEAR_EOL);
        buf.extend_from_slice(b"\r\n");
        row_count += 1;
    }

    while row_count < content_rows {
        buf.extend_from_slice(b"   ~");
        buf.extend_from_slice(CLEAR_EOL);
        buf.extend_from_slice(b"\r\n");
        row_count += 1;
    }

    separator_line(&mut buf, term_width);

    buf.extend_from_slice(REVERSE_VIDEO);
    let status = match editor.mode {
        EditorMode::ConfirmQuit => {
            format!(
                "{:term_width$}",
                "  Unsaved changes! Press Ctrl+C to quit without saving, or any other key to cancel",
            )
        }
        EditorMode::Message => {
            format!("{:term_width$}", format!("  {}", editor.message))
        }
        EditorMode::Normal => {
            format!(
                "{:term_width$}",
                "  ^O Save   ^X Exit   ^C Quit without saving",
            )
        }
    };
    buf.extend_from_slice(status.as_bytes());
    buf.extend_from_slice(RESET_ATTRS);

    let (cursor_row, cursor_col) = editor.buffer.screen_cursor();
    let cursor_pos = format!("\x1b[{cursor_row};{cursor_col}H");
    buf.extend_from_slice(cursor_pos.as_bytes());

    buf.extend_from_slice(CURSOR_SHOW);

    buf
}

async fn render_screen<W: Write>(writer: &mut W, editor: &Editor) -> Result<(), W::Error> {
    let buf = build_screen_buffer(editor);
    writer.write_all(&buf).await?;
    writer.flush().await?;
    Ok(())
}

// ============================================================================
// Input Handling
// ============================================================================

async fn handle_input<S: Write, F: EditorFs>(
    editor: &mut Editor,
    event: InputEvent,
    stream: &mut S,
    fs: &F,
) -> Result<(), S::Error> {
    match editor.mode {
        EditorMode::ConfirmQuit => match event {
            InputEvent::CtrlC => {
                editor.running = false;
            }
            _ => {
                editor.mode = EditorMode::Normal;
            }
        },
        EditorMode::Message => {
            editor.clear_message();
            match event {
                InputEvent::CtrlO | InputEvent::CtrlX | InputEvent::CtrlC => {
                    return handle_normal_input(editor, event, stream, fs).await;
                }
                _ => {}
            }
        }
        EditorMode::Normal => {
            return handle_normal_input(editor, event, stream, fs).await;
        }
    }
    Ok(())
}

async fn handle_normal_input<S: Write, F: EditorFs>(
    editor: &mut Editor,
    event: InputEvent,
    _stream: &mut S,
    fs: &F,
) -> Result<(), S::Error> {
    match event {
        InputEvent::Char(c) => editor.buffer.insert_char(c),
        InputEvent::Up => editor.buffer.move_up(),
        InputEvent::Down => editor.buffer.move_down(),
        InputEvent::Left => editor.buffer.move_left(),
        InputEvent::Right => editor.buffer.move_right(),
        InputEvent::Home | InputEvent::CtrlA => editor.buffer.move_home(),
        InputEvent::End | InputEvent::CtrlE => editor.buffer.move_end(),
        InputEvent::Backspace => editor.buffer.delete_char_before(),
        InputEvent::Delete => editor.buffer.delete_char_at(),
        InputEvent::Enter => editor.buffer.insert_newline(),
        InputEvent::CtrlO => {
            match editor.buffer.save(fs).await {
                Ok(bytes) => {
                    let msg = format!("Saved {bytes} bytes to {}", editor.buffer.filepath);
                    editor.set_message(&msg);
                }
                Err(e) => {
                    let msg = format!("Error: {e}");
                    editor.set_message(&msg);
                }
            }
        }
        InputEvent::CtrlX => {
            if editor.buffer.modified {
                editor.mode = EditorMode::ConfirmQuit;
            } else {
                editor.running = false;
            }
        }
        InputEvent::CtrlC => {
            editor.running = false;
        }
        InputEvent::None | InputEvent::Resize => {}
    }
    Ok(())
}

// ============================================================================
// Main Editor Entry Point
// ============================================================================

/// Run the neko editor.
///
/// Takes over the terminal until the user exits.
/// `stream` provides terminal I/O and size; `fs` provides file load/save.
pub async fn run<S: Read + Write + TermSizeProvider, F: EditorFs>(
    stream: &mut S,
    fs: &F,
    filepath: Option<&str>,
) -> Result<(), &'static str> {
    let term_size = stream.get_term_size();
    let mut editor = Editor::new(filepath.unwrap_or(""), term_size);

    if filepath.is_some() {
        let _ = editor.load(fs).await?;
    }

    let _ = stream.write_all(CLEAR_SCREEN).await;

    let mut parser = InputParser::new();
    let mut read_buf = [0u8; 32];

    while editor.running {
        let new_size = stream.get_term_size();
        if new_size != editor.term_size {
            editor.update_term_size(new_size);
        }

        if render_screen(stream, &editor).await.is_err() {
            return Err("Failed to render screen");
        }

        match stream.read(&mut read_buf).await {
            Ok(0) => break,
            Ok(n) => {
                for &byte in &read_buf[..n] {
                    let event = parser.feed(byte);
                    if event == InputEvent::Resize {
                        continue;
                    }
                    if handle_input(&mut editor, event, stream, fs).await.is_err() {
                        return Err("Failed to handle input");
                    }
                }
            }
            Err(_) => {
                return Err("Failed to read input");
            }
        }
    }

    let _ = stream.write_all(CLEAR_SCREEN).await;
    let _ = stream.write_all(CURSOR_HOME).await;
    let _ = stream.write_all(CURSOR_SHOW).await;
    let _ = stream.flush().await;

    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    const fn term(w: usize, h: usize) -> TermSize {
        TermSize { width: w, height: h }
    }

    fn buf(text: &str) -> EditorBuffer {
        let mut b = EditorBuffer::new("test.txt", term(80, 24));
        if !text.is_empty() {
            b.lines = text.lines().map(String::from).collect();
            if b.lines.is_empty() {
                b.lines.push(String::new());
            }
        }
        b
    }

    // -- EditorBuffer tests --------------------------------------------------

    #[test]
    fn new_buffer_has_one_empty_line() {
        let b = EditorBuffer::new("", term(80, 24));
        assert_eq!(b.line_count(), 1);
        assert_eq!(b.line_text(0), "");
        assert_eq!(b.cursor(), (0, 0));
        assert!(!b.is_modified());
    }

    #[test]
    fn insert_char_basic() {
        let mut b = buf("");
        b.insert_char('H');
        b.insert_char('i');
        assert_eq!(b.line_text(0), "Hi");
        assert_eq!(b.cursor(), (0, 2));
        assert!(b.is_modified());
    }

    #[test]
    fn insert_char_mid_line() {
        let mut b = buf("ac");
        b.cursor_col = 1;
        b.insert_char('b');
        assert_eq!(b.line_text(0), "abc");
        assert_eq!(b.cursor(), (0, 2));
    }

    #[test]
    fn backspace_within_line() {
        let mut b = buf("abc");
        b.cursor_col = 2;
        b.delete_char_before();
        assert_eq!(b.line_text(0), "ac");
        assert_eq!(b.cursor(), (0, 1));
    }

    #[test]
    fn backspace_joins_lines() {
        let mut b = buf("hello\nworld");
        b.cursor_row = 1;
        b.cursor_col = 0;
        b.delete_char_before();
        assert_eq!(b.line_count(), 1);
        assert_eq!(b.line_text(0), "helloworld");
        assert_eq!(b.cursor(), (0, 5));
    }

    #[test]
    fn backspace_at_start_does_nothing() {
        let mut b = buf("hello");
        b.delete_char_before();
        assert_eq!(b.line_text(0), "hello");
        assert_eq!(b.cursor(), (0, 0));
    }

    #[test]
    fn delete_within_line() {
        let mut b = buf("abc");
        b.cursor_col = 1;
        b.delete_char_at();
        assert_eq!(b.line_text(0), "ac");
        assert_eq!(b.cursor(), (0, 1));
    }

    #[test]
    fn delete_joins_lines() {
        let mut b = buf("hello\nworld");
        b.cursor_col = 5;
        b.delete_char_at();
        assert_eq!(b.line_count(), 1);
        assert_eq!(b.line_text(0), "helloworld");
    }

    #[test]
    fn insert_newline_splits() {
        let mut b = buf("helloworld");
        b.cursor_col = 5;
        b.insert_newline();
        assert_eq!(b.line_count(), 2);
        assert_eq!(b.line_text(0), "hello");
        assert_eq!(b.line_text(1), "world");
        assert_eq!(b.cursor(), (1, 0));
    }

    #[test]
    fn cursor_movement() {
        let mut b = buf("abc\ndef\nghi");

        b.move_down();
        assert_eq!(b.cursor(), (1, 0));

        b.move_right();
        b.move_right();
        assert_eq!(b.cursor(), (1, 2));

        b.move_up();
        assert_eq!(b.cursor(), (0, 2));

        b.move_left();
        assert_eq!(b.cursor(), (0, 1));
    }

    #[test]
    fn move_right_wraps_to_next_line() {
        let mut b = buf("ab\ncd");
        b.cursor_col = 2;
        b.move_right();
        assert_eq!(b.cursor(), (1, 0));
    }

    #[test]
    fn move_left_wraps_to_prev_line() {
        let mut b = buf("ab\ncd");
        b.cursor_row = 1;
        b.cursor_col = 0;
        b.move_left();
        assert_eq!(b.cursor(), (0, 2));
    }

    #[test]
    fn home_and_end() {
        let mut b = buf("hello");
        b.cursor_col = 3;
        b.move_home();
        assert_eq!(b.cursor(), (0, 0));
        b.move_end();
        assert_eq!(b.cursor(), (0, 5));
    }

    #[test]
    fn cursor_clamps_on_vertical_movement() {
        let mut b = buf("longline\nhi");
        b.cursor_col = 7;
        b.move_down();
        assert_eq!(b.cursor(), (1, 2));
    }

    #[test]
    fn scroll_down() {
        let mut b = EditorBuffer::new("", term(80, 8));
        b.lines = (0..20).map(|i| format!("line {i}")).collect();
        for _ in 0..6 {
            b.move_down();
        }
        assert_eq!(b.cursor_row, 6);
        assert!(b.scroll_offset > 0);
        assert!(b.cursor_row < b.scroll_offset + b.term_size.content_rows());
    }

    #[test]
    fn scroll_up() {
        let mut b = EditorBuffer::new("", term(80, 8));
        b.lines = (0..20).map(|i| format!("line {i}")).collect();
        b.cursor_row = 10;
        b.scroll_offset = 10;
        b.move_up();
        assert!(b.cursor_row >= b.scroll_offset);
    }

    // -- InputParser tests ---------------------------------------------------

    #[test]
    fn parse_printable_chars() {
        let mut p = InputParser::new();
        assert_eq!(p.feed(b'a'), InputEvent::Char('a'));
        assert_eq!(p.feed(b'Z'), InputEvent::Char('Z'));
        assert_eq!(p.feed(b' '), InputEvent::Char(' '));
    }

    #[test]
    fn parse_control_keys() {
        let mut p = InputParser::new();
        assert_eq!(p.feed(0x03), InputEvent::CtrlC);
        assert_eq!(p.feed(0x0F), InputEvent::CtrlO);
        assert_eq!(p.feed(0x18), InputEvent::CtrlX);
        assert_eq!(p.feed(0x01), InputEvent::CtrlA);
        assert_eq!(p.feed(0x05), InputEvent::CtrlE);
    }

    #[test]
    fn parse_enter_and_backspace() {
        let mut p = InputParser::new();
        assert_eq!(p.feed(0x0D), InputEvent::Enter);
        assert_eq!(p.feed(0x0A), InputEvent::Enter);
        assert_eq!(p.feed(0x7F), InputEvent::Backspace);
        assert_eq!(p.feed(0x08), InputEvent::Backspace);
    }

    #[test]
    fn parse_arrow_keys() {
        let mut p = InputParser::new();
        assert_eq!(p.feed(0x1b), InputEvent::None);
        assert_eq!(p.feed(b'['), InputEvent::None);
        assert_eq!(p.feed(b'A'), InputEvent::Up);

        assert_eq!(p.feed(0x1b), InputEvent::None);
        assert_eq!(p.feed(b'['), InputEvent::None);
        assert_eq!(p.feed(b'B'), InputEvent::Down);

        assert_eq!(p.feed(0x1b), InputEvent::None);
        assert_eq!(p.feed(b'['), InputEvent::None);
        assert_eq!(p.feed(b'C'), InputEvent::Right);

        assert_eq!(p.feed(0x1b), InputEvent::None);
        assert_eq!(p.feed(b'['), InputEvent::None);
        assert_eq!(p.feed(b'D'), InputEvent::Left);
    }

    #[test]
    fn parse_home_end() {
        let mut p = InputParser::new();
        assert_eq!(p.feed(0x1b), InputEvent::None);
        assert_eq!(p.feed(b'['), InputEvent::None);
        assert_eq!(p.feed(b'H'), InputEvent::Home);

        assert_eq!(p.feed(0x1b), InputEvent::None);
        assert_eq!(p.feed(b'['), InputEvent::None);
        assert_eq!(p.feed(b'F'), InputEvent::End);
    }

    #[test]
    fn parse_delete_key() {
        let mut p = InputParser::new();
        assert_eq!(p.feed(0x1b), InputEvent::None);
        assert_eq!(p.feed(b'['), InputEvent::None);
        assert_eq!(p.feed(b'3'), InputEvent::None);
        assert_eq!(p.feed(b'~'), InputEvent::Delete);
    }

    #[test]
    fn parse_extended_home_end() {
        let mut p = InputParser::new();
        assert_eq!(p.feed(0x1b), InputEvent::None);
        assert_eq!(p.feed(b'['), InputEvent::None);
        assert_eq!(p.feed(b'1'), InputEvent::None);
        assert_eq!(p.feed(b'~'), InputEvent::Home);

        assert_eq!(p.feed(0x1b), InputEvent::None);
        assert_eq!(p.feed(b'['), InputEvent::None);
        assert_eq!(p.feed(b'4'), InputEvent::None);
        assert_eq!(p.feed(b'~'), InputEvent::End);
    }

    #[test]
    fn parse_resize_signal() {
        let mut p = InputParser::new();
        assert_eq!(p.feed(0x00), InputEvent::Resize);
    }

    #[test]
    fn parser_resets_after_unknown_escape() {
        let mut p = InputParser::new();
        assert_eq!(p.feed(0x1b), InputEvent::None);
        assert_eq!(p.feed(b'X'), InputEvent::None);
        assert_eq!(p.feed(b'a'), InputEvent::Char('a'));
    }

    // -- Rendering tests -----------------------------------------------------

    #[test]
    fn render_shows_filename() {
        let editor = Editor::new("hello.txt", term(80, 24));
        let screen = build_screen_buffer(&editor);
        let output = String::from_utf8_lossy(&screen);
        assert!(output.contains("neko - hello.txt"));
    }

    #[test]
    fn render_shows_new_file() {
        let editor = Editor::new("", term(80, 24));
        let screen = build_screen_buffer(&editor);
        let output = String::from_utf8_lossy(&screen);
        assert!(output.contains("[New File]"));
    }

    #[test]
    fn render_shows_modified() {
        let mut editor = Editor::new("test.txt", term(80, 24));
        editor.buffer.insert_char('x');
        let screen = build_screen_buffer(&editor);
        let output = String::from_utf8_lossy(&screen);
        assert!(output.contains("[Modified]"));
    }

    #[test]
    fn render_shows_line_numbers() {
        let mut editor = Editor::new("", term(80, 24));
        editor.buffer.lines = vec!["first".into(), "second".into()];
        let screen = build_screen_buffer(&editor);
        let output = String::from_utf8_lossy(&screen);
        assert!(output.contains("   1| first"));
        assert!(output.contains("   2| second"));
    }

    #[test]
    fn render_shows_shortcut_bar() {
        let editor = Editor::new("", term(80, 24));
        let screen = build_screen_buffer(&editor);
        let output = String::from_utf8_lossy(&screen);
        assert!(output.contains("^O Save"));
        assert!(output.contains("^X Exit"));
    }

    #[test]
    fn render_shows_tilde_for_empty_rows() {
        let editor = Editor::new("", term(80, 10));
        let screen = build_screen_buffer(&editor);
        let output = String::from_utf8_lossy(&screen);
        assert!(output.contains("   ~"));
    }

    #[test]
    fn render_confirm_quit_message() {
        let mut editor = Editor::new("test.txt", term(80, 24));
        editor.buffer.modified = true;
        editor.mode = EditorMode::ConfirmQuit;
        let screen = build_screen_buffer(&editor);
        let output = String::from_utf8_lossy(&screen);
        assert!(output.contains("Unsaved changes!"));
    }
}
