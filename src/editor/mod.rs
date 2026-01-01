//! Neko Text Editor
//!
//! A nano-like terminal text editor that runs over SSH.
//! Supports reading, editing, and writing files.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use embedded_io_async::{Read, Write};

use crate::async_fs;

// ============================================================================
// ANSI Escape Sequences
// ============================================================================

/// Move cursor to home and clear screen (combined for reliability)
const CLEAR_AND_HOME: &[u8] = b"\x1b[H\x1b[J";
/// Clear entire screen
const CLEAR_SCREEN: &[u8] = b"\x1b[2J";
/// Move cursor to home position (1,1)
const CURSOR_HOME: &[u8] = b"\x1b[H";
/// Hide cursor
const CURSOR_HIDE: &[u8] = b"\x1b[?25l";
/// Show cursor
const CURSOR_SHOW: &[u8] = b"\x1b[?25h";
/// Reverse video (for status bars)
const REVERSE_VIDEO: &[u8] = b"\x1b[7m";
/// Reset all attributes
const RESET_ATTRS: &[u8] = b"\x1b[0m";
/// Dim text (for line numbers)
const DIM_TEXT: &[u8] = b"\x1b[2m";
/// Clear to end of line
const CLEAR_EOL: &[u8] = b"\x1b[K";

/// Special byte used to signal a terminal resize event (matches ssh.rs)
const RESIZE_SIGNAL_BYTE: u8 = 0x00;

// ============================================================================
// Editor Configuration
// ============================================================================

/// Width reserved for line numbers (4 digits + "| " = 6 chars)
const LINE_NUM_WIDTH: usize = 6;

/// Terminal dimensions passed to the editor
#[derive(Clone, Copy, PartialEq)]
pub struct TermSize {
    pub width: usize,
    pub height: usize,
}

impl TermSize {
    /// Create a new TermSize with given dimensions
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width: width as usize,
            height: height as usize,
        }
    }

    /// Default terminal size (80x24)
    pub fn default() -> Self {
        Self {
            width: 80,
            height: 24,
        }
    }

    /// Number of content rows (total - title - separator*2 - status)
    fn content_rows(&self) -> usize {
        if self.height > 4 {
            self.height - 4
        } else {
            1
        }
    }
}

/// Trait for streams that can provide terminal size information
pub trait TermSizeProvider {
    /// Get the current terminal size
    fn get_term_size(&self) -> TermSize;
}

// ============================================================================
// Editor Buffer
// ============================================================================

/// Manages the document content and cursor state
pub struct EditorBuffer {
    /// Lines of text (each line without newline)
    lines: Vec<String>,
    /// Cursor row (0-indexed, relative to document)
    cursor_row: usize,
    /// Cursor column (0-indexed)
    cursor_col: usize,
    /// First visible row (for scrolling)
    scroll_offset: usize,
    /// Whether the buffer has been modified
    modified: bool,
    /// File path
    filepath: String,
    /// Terminal size
    term_size: TermSize,
}

impl EditorBuffer {
    /// Create a new empty buffer
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

    /// Load content from a file
    /// Returns the number of lines loaded, or 0 if file doesn't exist (new file)
    pub async fn load(&mut self) -> Result<usize, &'static str> {
        if self.filepath.is_empty() {
            return Ok(0);
        }

        match async_fs::read_to_string(&self.filepath).await {
            Ok(content) => {
                self.lines = content
                    .lines()
                    .map(|s| String::from(s))
                    .collect();
                let line_count = self.lines.len();
                if self.lines.is_empty() {
                    self.lines.push(String::new());
                }
                self.cursor_row = 0;
                self.cursor_col = 0;
                self.scroll_offset = 0;
                self.modified = false;
                Ok(line_count)
            }
            Err(_) => {
                // File doesn't exist - start with empty buffer (new file)
                self.lines = vec![String::new()];
                self.modified = false;
                Ok(0)
            }
        }
    }

    /// Save content to file
    pub async fn save(&mut self) -> Result<usize, &'static str> {
        if self.filepath.is_empty() {
            return Err("No filename specified");
        }

        let content = self.lines.join("\n");
        let bytes = content.as_bytes();

        match async_fs::write_file(&self.filepath, bytes).await {
            Ok(()) => {
                self.modified = false;
                Ok(bytes.len())
            }
            Err(_) => Err("Failed to write file"),
        }
    }

    /// Get the current line
    fn current_line(&self) -> &String {
        &self.lines[self.cursor_row]
    }

    /// Get mutable reference to current line
    fn current_line_mut(&mut self) -> &mut String {
        &mut self.lines[self.cursor_row]
    }

    /// Insert a character at cursor position
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

    /// Delete character before cursor (backspace)
    pub fn delete_char_before(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
            let row = self.cursor_row;
            let col = self.cursor_col;
            self.lines[row].remove(col);
            self.modified = true;
        } else if self.cursor_row > 0 {
            // Join with previous line
            let current = self.lines.remove(self.cursor_row);
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].len();
            self.lines[self.cursor_row].push_str(&current);
            self.modified = true;
            self.ensure_cursor_visible();
        }
    }

    /// Delete character at cursor (delete key)
    pub fn delete_char_at(&mut self) {
        let line_len = self.current_line().len();
        if self.cursor_col < line_len {
            let row = self.cursor_row;
            let col = self.cursor_col;
            self.lines[row].remove(col);
            self.modified = true;
        } else if self.cursor_row < self.lines.len() - 1 {
            // Join with next line
            let next = self.lines.remove(self.cursor_row + 1);
            self.lines[self.cursor_row].push_str(&next);
            self.modified = true;
        }
    }

    /// Insert a new line at cursor
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

    /// Move cursor up
    pub fn move_up(&mut self) {
        if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.clamp_cursor_col();
            self.ensure_cursor_visible();
        }
    }

    /// Move cursor down
    pub fn move_down(&mut self) {
        if self.cursor_row < self.lines.len() - 1 {
            self.cursor_row += 1;
            self.clamp_cursor_col();
            self.ensure_cursor_visible();
        }
    }

    /// Move cursor left
    pub fn move_left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].len();
            self.ensure_cursor_visible();
        }
    }

    /// Move cursor right
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

    /// Move to beginning of line
    pub fn move_home(&mut self) {
        self.cursor_col = 0;
    }

    /// Move to end of line
    pub fn move_end(&mut self) {
        self.cursor_col = self.current_line().len();
    }

    /// Clamp cursor column to line length
    fn clamp_cursor_col(&mut self) {
        let line_len = self.current_line().len();
        if self.cursor_col > line_len {
            self.cursor_col = line_len;
        }
    }

    /// Ensure cursor is visible by adjusting scroll
    fn ensure_cursor_visible(&mut self) {
        let content_rows = self.term_size.content_rows();
        if self.cursor_row < self.scroll_offset {
            self.scroll_offset = self.cursor_row;
        } else if self.cursor_row >= self.scroll_offset + content_rows {
            self.scroll_offset = self.cursor_row - content_rows + 1;
        }
    }

    /// Get visible lines for rendering
    fn visible_lines(&self) -> impl Iterator<Item = (usize, &String)> + '_ {
        let content_rows = self.term_size.content_rows();
        self.lines
            .iter()
            .enumerate()
            .skip(self.scroll_offset)
            .take(content_rows)
    }

    /// Get cursor position on screen (row, col)
    fn screen_cursor(&self) -> (usize, usize) {
        let row = self.cursor_row - self.scroll_offset + 3; // +3 for title bar, separator, and 1-based indexing
        let col = LINE_NUM_WIDTH + self.cursor_col + 1; // LINE_NUM_WIDTH already includes "| ", +1 for 1-based
        (row, col)
    }
}

// ============================================================================
// Editor State
// ============================================================================

/// Current editor mode
#[derive(Clone, Copy, PartialEq)]
enum EditorMode {
    /// Normal editing mode
    Normal,
    /// Confirm quit without saving
    ConfirmQuit,
    /// Show status message
    Message,
}

/// Main editor state
pub struct Editor {
    buffer: EditorBuffer,
    mode: EditorMode,
    message: String,
    running: bool,
    term_size: TermSize,
}

impl Editor {
    /// Create a new editor
    pub fn new(filepath: &str, term_size: TermSize) -> Self {
        Self {
            buffer: EditorBuffer::new(filepath, term_size),
            mode: EditorMode::Normal,
            message: String::new(),
            running: true,
            term_size,
        }
    }

    /// Load file into buffer, returns number of lines loaded
    pub async fn load(&mut self) -> Result<usize, &'static str> {
        let lines = self.buffer.load().await?;
        if lines > 0 {
            self.set_message(&format!("Loaded {} lines", lines));
        } else {
            self.set_message("New file");
        }
        Ok(lines)
    }

    /// Update the terminal size (called when window is resized)
    pub fn update_term_size(&mut self, new_size: TermSize) {
        self.term_size = new_size;
        self.buffer.term_size = new_size;
        // Ensure cursor is still visible with new dimensions
        self.buffer.ensure_cursor_visible();
    }

    /// Set a status message
    fn set_message(&mut self, msg: &str) {
        self.message = String::from(msg);
        self.mode = EditorMode::Message;
    }

    /// Clear status message
    fn clear_message(&mut self) {
        self.message.clear();
        self.mode = EditorMode::Normal;
    }
}

// ============================================================================
// Escape Sequence Parser
// ============================================================================

/// State for parsing escape sequences
#[derive(Clone, Copy, PartialEq)]
enum EscapeState {
    Normal,
    Escape,
    Bracket,
    Extended, // For sequences like ESC[3~
}

/// Parsed input event
enum InputEvent {
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
    None,
}

// ============================================================================
// Screen Rendering
// ============================================================================

/// Build the screen output into a buffer
fn build_screen_buffer(editor: &Editor) -> Vec<u8> {
    let mut buf = Vec::with_capacity(8192);
    let term_width = editor.term_size.width;
    let content_rows = editor.term_size.content_rows();

    // Reset all attributes first, hide cursor, move home, and clear screen
    buf.extend_from_slice(RESET_ATTRS);
    buf.extend_from_slice(CURSOR_HIDE);
    buf.extend_from_slice(CURSOR_HOME);
    buf.extend_from_slice(CLEAR_SCREEN);
    buf.extend_from_slice(CURSOR_HOME);

    // Title bar (reverse video = white on black)  
    buf.extend_from_slice(REVERSE_VIDEO);
    let title = if editor.buffer.filepath.is_empty() {
        format!("  neko - [New File]")
    } else if editor.buffer.modified {
        format!("  neko - {} [Modified]", editor.buffer.filepath)
    } else {
        format!("  neko - {}", editor.buffer.filepath)
    };
    // Pad title to full width
    let padded_title = format!("{:width$}", title, width = term_width);
    buf.extend_from_slice(padded_title.as_bytes());
    buf.extend_from_slice(RESET_ATTRS);
    buf.extend_from_slice(b"\r\n");

    // Separator line
    for _ in 0..term_width {
        buf.push(b'-');
    }
    buf.extend_from_slice(b"\r\n");

    // Content area
    let mut row_count = 0;
    for (line_num, line) in editor.buffer.visible_lines() {
        // Line number
        let line_num_str = format!("{:>4}| ", line_num + 1);
        buf.extend_from_slice(line_num_str.as_bytes());

        // Line content (truncate if too long)
        let available_width = term_width.saturating_sub(LINE_NUM_WIDTH);
        let display_line: String = line.chars().take(available_width).collect();
        buf.extend_from_slice(display_line.as_bytes());
        buf.extend_from_slice(CLEAR_EOL);
        buf.extend_from_slice(b"\r\n");
        row_count += 1;
    }

    // Fill remaining content rows with empty lines (tilde markers)
    while row_count < content_rows {
        buf.extend_from_slice(b"   ~");
        buf.extend_from_slice(CLEAR_EOL);
        buf.extend_from_slice(b"\r\n");
        row_count += 1;
    }

    // Bottom separator
    for _ in 0..term_width {
        buf.push(b'-');
    }
    buf.extend_from_slice(b"\r\n");

    // Status/Shortcut bar
    buf.extend_from_slice(REVERSE_VIDEO);
    let status = match editor.mode {
        EditorMode::ConfirmQuit => {
            format!("{:width$}", "  Unsaved changes! Press Ctrl+C to quit without saving, or any other key to cancel", width = term_width)
        }
        EditorMode::Message => {
            format!("{:width$}", format!("  {}", editor.message), width = term_width)
        }
        EditorMode::Normal => {
            format!("{:width$}", "  ^O Save   ^X Exit   ^C Quit without saving", width = term_width)
        }
    };
    buf.extend_from_slice(status.as_bytes());
    buf.extend_from_slice(RESET_ATTRS);

    // Position cursor (ANSI uses 1-based coordinates)
    let (cursor_row, cursor_col) = editor.buffer.screen_cursor();
    let cursor_pos = format!("\x1b[{};{}H", cursor_row, cursor_col);
    buf.extend_from_slice(cursor_pos.as_bytes());

    // Show cursor
    buf.extend_from_slice(CURSOR_SHOW);

    buf
}

/// Render the editor screen - writes all output in a single batch
async fn render_screen<W: Write>(writer: &mut W, editor: &Editor) -> Result<(), W::Error> {
    let buf = build_screen_buffer(editor);
    writer.write_all(&buf).await?;
    writer.flush().await?;
    Ok(())
}

// ============================================================================
// Input Handling
// ============================================================================

/// Process a single input event
async fn handle_input<S: Write>(
    editor: &mut Editor,
    event: InputEvent,
    stream: &mut S,
) -> Result<(), S::Error> {
    match editor.mode {
        EditorMode::ConfirmQuit => {
            match event {
                InputEvent::CtrlC => {
                    editor.running = false;
                }
                _ => {
                    editor.mode = EditorMode::Normal;
                }
            }
        }
        EditorMode::Message => {
            // Any input clears the message
            editor.clear_message();
            // Also process the input if it's a command
            match event {
                InputEvent::CtrlO | InputEvent::CtrlX | InputEvent::CtrlC => {
                    return handle_normal_input(editor, event, stream).await;
                }
                _ => {}
            }
        }
        EditorMode::Normal => {
            return handle_normal_input(editor, event, stream).await;
        }
    }
    Ok(())
}

/// Handle input in normal mode
async fn handle_normal_input<S: Write>(
    editor: &mut Editor,
    event: InputEvent,
    _stream: &mut S,
) -> Result<(), S::Error> {
    match event {
        InputEvent::Char(c) => {
            editor.buffer.insert_char(c);
        }
        InputEvent::Up => {
            editor.buffer.move_up();
        }
        InputEvent::Down => {
            editor.buffer.move_down();
        }
        InputEvent::Left => {
            editor.buffer.move_left();
        }
        InputEvent::Right => {
            editor.buffer.move_right();
        }
        InputEvent::Home | InputEvent::CtrlA => {
            editor.buffer.move_home();
        }
        InputEvent::End | InputEvent::CtrlE => {
            editor.buffer.move_end();
        }
        InputEvent::Backspace => {
            editor.buffer.delete_char_before();
        }
        InputEvent::Delete => {
            editor.buffer.delete_char_at();
        }
        InputEvent::Enter => {
            editor.buffer.insert_newline();
        }
        InputEvent::CtrlO => {
            // Save file
            match editor.buffer.save().await {
                Ok(bytes) => {
                    let msg = format!("Saved {} bytes to {}", bytes, editor.buffer.filepath);
                    editor.set_message(&msg);
                }
                Err(e) => {
                    let msg = format!("Error: {}", e);
                    editor.set_message(&msg);
                }
            }
        }
        InputEvent::CtrlX => {
            // Exit (prompt if modified)
            if editor.buffer.modified {
                editor.mode = EditorMode::ConfirmQuit;
            } else {
                editor.running = false;
            }
        }
        InputEvent::CtrlC => {
            // Force quit without saving
            editor.running = false;
        }
        InputEvent::None => {}
    }
    Ok(())
}

// ============================================================================
// Main Editor Entry Point
// ============================================================================

/// Run the neko editor
///
/// This function takes over the terminal until the user exits.
/// Uses a single stream that implements Read, Write, and TermSizeProvider.
pub async fn run<S: Read + Write + TermSizeProvider>(
    stream: &mut S,
    filepath: Option<&str>,
) -> Result<(), &'static str> {
    let term_size = stream.get_term_size();
    let mut editor = Editor::new(filepath.unwrap_or(""), term_size);

    // Load file if specified
    if filepath.is_some() {
        let _ = editor.load().await?;
    }

    // Clear screen initially
    let _ = stream.write_all(CLEAR_SCREEN).await;

    // Input parsing state
    let mut escape_state = EscapeState::Normal;
    let mut extended_char: u8 = 0;
    let mut read_buf = [0u8; 32];

    // Main editor loop
    while editor.running {
        // Check for terminal resize
        let new_size = stream.get_term_size();
        if new_size != editor.term_size {
            editor.update_term_size(new_size);
        }

        // Render screen
        if render_screen(stream, &editor).await.is_err() {
            return Err("Failed to render screen");
        }

        // Read input
        match stream.read(&mut read_buf).await {
            Ok(0) => {
                // EOF - exit editor
                break;
            }
            Ok(n) => {
                for &byte in &read_buf[..n] {
                    // Check for resize signal first
                    if byte == RESIZE_SIGNAL_BYTE {
                        // Resize signal received - just continue to trigger resize check
                        continue;
                    }
                    
                    let event = match escape_state {
                        EscapeState::Normal => {
                            match byte {
                                0x1b => {
                                    escape_state = EscapeState::Escape;
                                    InputEvent::None
                                }
                                0x01 => InputEvent::CtrlA,  // Ctrl+A
                                0x03 => InputEvent::CtrlC,  // Ctrl+C
                                0x05 => InputEvent::CtrlE,  // Ctrl+E
                                0x0F => InputEvent::CtrlO,  // Ctrl+O
                                0x18 => InputEvent::CtrlX,  // Ctrl+X
                                0x0D | 0x0A => InputEvent::Enter,
                                0x7F | 0x08 => InputEvent::Backspace,
                                c if c >= 0x20 && c < 0x7F => {
                                    InputEvent::Char(c as char)
                                }
                                _ => InputEvent::None,
                            }
                        }
                        EscapeState::Escape => {
                            if byte == b'[' {
                                escape_state = EscapeState::Bracket;
                                InputEvent::None
                            } else {
                                escape_state = EscapeState::Normal;
                                InputEvent::None
                            }
                        }
                        EscapeState::Bracket => {
                            escape_state = EscapeState::Normal;
                            match byte {
                                b'A' => InputEvent::Up,
                                b'B' => InputEvent::Down,
                                b'C' => InputEvent::Right,
                                b'D' => InputEvent::Left,
                                b'H' => InputEvent::Home,
                                b'F' => InputEvent::End,
                                b'3' => {
                                    // Might be Delete (ESC[3~)
                                    extended_char = byte;
                                    escape_state = EscapeState::Extended;
                                    InputEvent::None
                                }
                                b'1' | b'4' | b'7' | b'8' => {
                                    // Home/End variants
                                    extended_char = byte;
                                    escape_state = EscapeState::Extended;
                                    InputEvent::None
                                }
                                _ => InputEvent::None,
                            }
                        }
                        EscapeState::Extended => {
                            escape_state = EscapeState::Normal;
                            if byte == b'~' {
                                match extended_char {
                                    b'3' => InputEvent::Delete,
                                    b'1' | b'7' => InputEvent::Home,
                                    b'4' | b'8' => InputEvent::End,
                                    _ => InputEvent::None,
                                }
                            } else {
                                InputEvent::None
                            }
                        }
                    };

                    if let Err(_) = handle_input(&mut editor, event, stream).await {
                        return Err("Failed to handle input");
                    }
                }
            }
            Err(_) => {
                return Err("Failed to read input");
            }
        }
    }

    // Clear screen on exit
    let _ = stream.write_all(CLEAR_SCREEN).await;
    let _ = stream.write_all(CURSOR_HOME).await;
    let _ = stream.write_all(CURSOR_SHOW).await;
    let _ = stream.flush().await;

    Ok(())
}

