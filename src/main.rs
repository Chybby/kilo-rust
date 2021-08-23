use nix::sys::termios::{
    self, ControlFlags, InputFlags, LocalFlags, OutputFlags, SetArg, Termios,
};
use regex::Regex;
use std::cmp;
use std::env;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::io::AsRawFd;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Instant;
use unicode_width::UnicodeWidthChar;

const VERSION: &str = "0.0.1";

// TODO: get config from config file.
const TAB_STOP: usize = 8;
const MAX_STATUS_FILENAME_LENGTH: usize = 20;
const QUIT_TIMES: u8 = 3;
const NON_PRINTING_CHARACTERS: bool = false;

// Create a way to read chars from stdin without blocking.
fn spawn_stdin_channel() -> Receiver<char> {
    let (tx, rx) = mpsc::channel::<char>();
    thread::spawn(move || loop {
        let mut byte: [u8; 1] = [0];
        let mut buf: [u8; 4] = [0; 4];
        let mut i = 0;
        loop {
            io::stdin().read_exact(&mut byte).unwrap();
            buf[i] = byte[0];
            if let Ok(s) = std::str::from_utf8(&buf[0..i + 1]) {
                tx.send(s.chars().next().unwrap()).unwrap();
                break;
            }
            i += 1;
        }
    });
    rx
}

fn get_window_size() -> Dimensions {
    // Interfacing with ioctl in Rust is a bit of a pain.
    let (width, height) = term_size::dimensions_stdin()
        .expect("Failed to get terminal dimensions.");
    Dimensions {
        rows: height,
        cols: width,
    }
}

#[derive(Copy, Clone)]
struct Position {
    x: usize,
    y: usize,
}

struct Dimensions {
    rows: usize,
    cols: usize,
}

enum KeypressResult {
    Continue,
    Terminate,
}

#[derive(Debug)]
enum Arrow {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug)]
enum Key {
    Char(char),
    Ctrl(char),
    Arrow(Arrow),
    PageUp,
    PageDown,
    Home,
    End,
    Delete,
    Backspace,
    Esc,
    Enter,
}

struct Row {
    chars: String,
    render: String,
}

impl Row {
    fn update(&mut self) {
        self.render = String::new();

        let mut render_length = 0;

        for c in self.chars.chars() {
            if c == '\t' {
                let mut tab_size = TAB_STOP - (render_length % TAB_STOP);
                while tab_size > 0 {
                    if !NON_PRINTING_CHARACTERS {
                        self.render.push(' ');
                    } else if tab_size == 1 {
                        self.render.push('→');
                    } else {
                        self.render.push('—');
                    }
                    render_length += 1;
                    tab_size -= 1;
                }
            } else if c == ' ' {
                if NON_PRINTING_CHARACTERS {
                    self.render.push('·');
                } else {
                    self.render.push(' ');
                }
                render_length += 1;
            } else {
                self.render.push(c);
                render_length += 1;
                for _ in 0..UnicodeWidthChar::width(c).unwrap_or(1) - 1 {
                    self.render.push('\x00');
                    render_length += 1;
                }
            }
        }
        if NON_PRINTING_CHARACTERS {
            self.render.push('↵');
        }
    }

    fn insert_char(&mut self, mut index: usize, c: char) {
        let count = self.chars.chars().count();
        if index > count {
            index = count;
        }

        let mut new_chars = String::new();

        if index == count {
            self.chars.push(c);
        } else {
            for (i, char) in self.chars.chars().enumerate() {
                if i == index {
                    new_chars.push(c);
                }
                new_chars.push(char);
            }

            self.chars = new_chars;
        }

        self.update();
    }

    fn append_string(&mut self, s: &str) {
        self.chars.push_str(s);
        self.update();
    }

    fn delete_char(&mut self, index: usize) {
        let count = self.chars.chars().count();
        if index >= count {
            return;
        }

        let mut new_chars = String::new();

        for (i, char) in self.chars.chars().enumerate() {
            if i != index {
                new_chars.push(char);
            }
        }

        self.chars = new_chars;
        self.update();
    }
}

struct Editor {
    screen_dimensions: Dimensions,
    cursor_position: Position,
    cursor_render_x: usize,
    input: Receiver<char>,
    text_offset: Position,
    rows: Vec<Row>,
    filename: Option<String>,
    status_message: String,
    status_message_time: Instant,
    dirty: bool,
    quit_times: u8,
    matches: Vec<usize>,
    match_index: usize,
}

impl Editor {
    fn new() -> Editor {
        let mut screen_dimensions = get_window_size();
        screen_dimensions.rows -= 2; // Make room for status bar and message.

        Editor {
            screen_dimensions,
            cursor_position: Position { x: 0, y: 0 },
            cursor_render_x: 0,
            input: spawn_stdin_channel(),
            text_offset: Position { x: 0, y: 0 },
            rows: Vec::new(),
            filename: None,
            status_message: String::new(),
            status_message_time: Instant::now(),
            dirty: false,
            quit_times: QUIT_TIMES,
            matches: Vec::new(),
            match_index: 0,
        }
    }

    // *** Row Operations ***

    fn insert_row(&mut self, index: usize, chars: &str) {
        if index > self.rows.len() {
            return;
        }

        let mut row = Row {
            chars: chars.to_string(),
            render: String::new(),
        };
        row.update();
        self.rows.insert(index, row);
        self.dirty = true;
    }

    fn get_render_index(&self) -> usize {
        if self.cursor_position.y >= self.rows.len()
            || self.cursor_position.x == 0
        {
            return 0;
        }

        let mut render_index = 0;

        for c in self
            .get_current_row()
            .unwrap()
            .chars
            .chars()
            .take(self.cursor_position.x)
        {
            if c == '\t' {
                render_index += (TAB_STOP - 1) - (render_index % TAB_STOP);
            } else {
                render_index += UnicodeWidthChar::width(c).unwrap_or(0);
            }
        }
        render_index
    }

    fn get_char_index(&self) -> usize {
        if self.cursor_position.y >= self.rows.len()
            || self.cursor_render_x == 0
        {
            return 0;
        }

        let mut char_index = 0;
        let mut render_index = 0;

        for c in self.get_current_row().unwrap().chars.chars() {
            if c == '\t' {
                render_index += (TAB_STOP - 1) - (render_index % TAB_STOP);
            }
            render_index += 1;
            char_index += 1;
            if render_index >= self.cursor_render_x {
                return char_index;
            }
        }
        char_index
    }

    fn get_current_row(&self) -> Option<&Row> {
        if self.cursor_position.y >= self.rows.len() {
            None
        } else {
            Some(&self.rows[self.cursor_position.y])
        }
    }

    // *** Editor Operations ***

    fn insert_char(&mut self, c: char) {
        if self.cursor_position.y == self.rows.len() {
            self.insert_row(self.rows.len(), "");
        }
        self.rows[self.cursor_position.y]
            .insert_char(self.cursor_position.x, c);
        self.cursor_position.x += 1;
        self.dirty = true;
    }

    fn delete_char(&mut self) {
        if self.cursor_position.x == 0 && self.cursor_position.y == 0 {
            return;
        }
        if self.cursor_position.y == self.rows.len() {
            return;
        }

        if self.cursor_position.x > 0 {
            self.rows[self.cursor_position.y]
                .delete_char(self.cursor_position.x - 1);
            self.cursor_position.x -= 1;
            self.dirty = true;
        } else {
            self.cursor_position.x =
                self.rows[self.cursor_position.y - 1].chars.chars().count();
            let (start, end) = self.rows.split_at_mut(self.cursor_position.y);
            let previous_row = start.last_mut().unwrap();
            let current_row = &end[0];
            previous_row.append_string(&current_row.chars);
            self.delete_row(self.cursor_position.y);
            self.cursor_position.y -= 1;
        }
    }

    fn insert_newline(&mut self) {
        if self.cursor_position.x == 0 {
            self.insert_row(self.cursor_position.y, "");
        } else {
            let new_row_contents = self.rows[self.cursor_position.y]
                .chars
                .split_at(self.cursor_position.x)
                .1
                .to_string();
            self.insert_row(self.cursor_position.y + 1, &new_row_contents);
            self.rows[self.cursor_position.y]
                .chars
                .truncate(self.cursor_position.x);
            self.rows[self.cursor_position.y].update();
        }
        self.cursor_position.y += 1;
        self.cursor_position.x = 0;
    }

    fn delete_row(&mut self, index: usize) {
        if index >= self.rows.len() {
            return;
        }

        self.rows.remove(index);
        self.dirty = true;
    }

    // *** File I/O ***

    fn open(&mut self, filename: &str) {
        let f = File::open(filename).expect("Failed to open file");
        let reader = BufReader::new(f);
        let lines = reader.lines();

        for l in lines {
            self.insert_row(self.rows.len(), &l.expect("Error reading file"));
        }

        self.filename = Some(filename.to_string());
        self.dirty = false;
    }

    fn rows_to_string(&self) -> String {
        let mut file_contents = String::new();

        for row in &self.rows {
            file_contents.push_str(&row.chars);
            file_contents.push('\n');
        }
        file_contents
    }

    fn save(&mut self) {
        if self.filename.is_none() {
            self.filename = self
                .prompt("Save as: {} (ESC to cancel)", |_, _, _| String::new());
            if self.filename.is_none() {
                self.set_status_message("Save aborted");
                return;
            }
        }

        match File::create(self.filename.as_ref().unwrap()) {
            Ok(mut file) => {
                let file_contents = self.rows_to_string();
                match file.write_all(file_contents.as_bytes()) {
                    Ok(_) => {
                        self.set_status_message(&format!(
                            "{} bytes written",
                            file_contents.len()
                        ));
                        self.dirty = false;
                    }
                    // An error here means the file contents are lost. Oh well.
                    Err(error) => self
                        .set_status_message(&format!("Save failed: {}", error)),
                }
            }
            Err(error) => {
                self.set_status_message(&format!("Save failed: {:?}", error))
            }
        }
    }

    // *** Find ***

    fn find_callback(&mut self, query: &str, key: Key) -> String {
        if query.is_empty() {
            return String::new();
        }

        let regex: Regex;
        match Regex::new(query) {
            Ok(re) => regex = re,
            _ => return ": Invalid regex".to_string(),
        }

        match key {
            Key::Esc | Key::Enter => {
                self.matches.clear();
                self.match_index = 0;
                return String::new();
            }
            Key::Arrow(Arrow::Left) | Key::Arrow(Arrow::Up) => {
                self.match_index = if self.match_index == 0 {
                    self.matches.len() - 1
                } else {
                    self.match_index - 1
                };
            }
            Key::Arrow(Arrow::Right) | Key::Arrow(Arrow::Down) => {
                self.match_index = if self.match_index == self.matches.len() - 1
                {
                    0
                } else {
                    self.match_index + 1
                };
            }
            _ => {
                self.matches.clear();
                self.match_index = 0;
                for (i, row) in self.rows.iter().enumerate() {
                    if regex.is_match(&row.chars) {
                        self.matches.push(i);
                    }
                }
            }
        }

        if self.matches.is_empty() {
            return ": No results".to_string();
        }

        let row = &self.rows[self.matches[self.match_index]];
        let row_index = regex.find(&row.chars).unwrap();
        self.cursor_position.y = self.matches[self.match_index];
        self.text_offset.y = self.matches[self.match_index];
        for (i, (byte_offset, _)) in row.chars.char_indices().enumerate() {
            if byte_offset >= row_index.start() {
                self.cursor_position.x = i;
                break;
            }
        }

        format!(
            ": {} out of {} results",
            self.match_index + 1,
            self.matches.len()
        )
    }

    fn find(&mut self) {
        let saved_cursor_position = self.cursor_position;
        let saved_text_offset = self.text_offset;

        if self
            .prompt("Search: {} (Use ESC/Arrows/Enter)", Editor::find_callback)
            .is_none()
        {
            self.cursor_position = saved_cursor_position;
            self.text_offset = saved_text_offset;
        }
    }

    // *** Output ***

    fn clear_screen(contents: &mut String) {
        // Clear the whole screen.
        contents.push_str("\x1b[2J");
    }

    fn clear_row(contents: &mut String) {
        // Clear the current row from the cursor to the end.
        contents.push_str("\x1b[K");
    }

    fn draw_cursor(contents: &mut String, cursor_position: &Position) {
        // Move the displayed cursor to a certain position.
        let s = format!(
            "\x1b[{};{}H",
            cursor_position.y + 1,
            cursor_position.x + 1
        );
        contents.push_str(&s);
    }

    fn reset_cursor(contents: &mut String) {
        // Move the cursor to the top left.
        Editor::draw_cursor(contents, &Position { x: 0, y: 0 });
    }

    fn hide_cursor(contents: &mut String) {
        // Make the cursor invisible.
        contents.push_str("\x1b[?25l");
    }

    fn show_cursor(contents: &mut String) {
        // Make the cursor visible.
        contents.push_str("\x1b[?25h");
    }

    fn draw_rows(&self, contents: &mut String) {
        for y in 0..self.screen_dimensions.rows {
            let mut filled_line = false;
            let file_row = y + self.text_offset.y;
            if file_row >= self.rows.len() {
                if self.rows.is_empty() && y == self.screen_dimensions.rows / 3
                {
                    let welcome_message =
                        format!("Kilo editor -- version {}", VERSION);
                    let message_length = cmp::min(
                        welcome_message.len(),
                        self.screen_dimensions.cols - 1,
                    );

                    let mut padding =
                        (self.screen_dimensions.cols - message_length) / 2;
                    if padding > 0 {
                        // TODO: colour.
                        contents.push('~');
                        padding -= 1;
                    }

                    for _ in 0..padding {
                        contents.push(' ');
                    }

                    contents.push_str(&welcome_message[..message_length]);
                } else {
                    // TODO: colour.
                    contents.push('~');
                }
            } else {
                let line_length = self.rows[file_row].render.chars().count();
                // Check if any of this line is visible.
                if self.text_offset.x < line_length {
                    let mut displayed_length = line_length - self.text_offset.x;
                    // Cap the displayed length to the length of the screen.
                    if displayed_length >= self.screen_dimensions.cols {
                        displayed_length = self.screen_dimensions.cols;
                        filled_line = true;
                    }
                    // Start displaying the line at the text offset.
                    let start_index = self.text_offset.x;
                    let row = self.rows[file_row]
                        .render
                        .chars()
                        .skip(start_index)
                        .take(displayed_length)
                        .collect::<String>();
                    if row.starts_with('\x00') {
                        // TODO: colour.
                        contents.push('<');
                    }
                    contents.push_str(&row);
                    if self.rows[file_row]
                        .render
                        .chars()
                        .nth(start_index + displayed_length)
                        .unwrap_or('a')
                        == '\x00'
                    {
                        contents.pop();
                        // TODO: colour.
                        contents.push('>');
                    }
                }
            }
            if !filled_line {
                Editor::clear_row(contents);
            }

            contents.push_str("\r\n");
        }
    }

    fn draw_status_bar(&self, contents: &mut String) {
        contents.push_str("\x1b[7m"); // Invert colours.

        let filename = match &self.filename {
            Some(filename) => {
                if filename.len() > MAX_STATUS_FILENAME_LENGTH {
                    &filename[0..MAX_STATUS_FILENAME_LENGTH]
                } else {
                    filename
                }
            }
            None => "[No name]",
        };

        let left_status = format!(
            "{} - {} lines {}",
            filename,
            self.rows.len(),
            if self.dirty { "(modified)" } else { "" }
        );

        let right_status = format!(
            "{}:{} ",
            self.cursor_position.y + 1,
            self.cursor_position.x + 1
        );

        let mut status: String = format!(
            "{:width$}",
            left_status,
            width = self.screen_dimensions.cols - right_status.len()
        )
        .to_string();

        status.push_str(&right_status);

        if status.len() > self.screen_dimensions.cols {
            contents.push_str(&status[0..self.screen_dimensions.cols]);
        } else {
            contents.push_str(&status);
        }

        contents.push_str("\x1b[m"); // Un-invert colours.
        contents.push_str("\r\n");
    }

    fn draw_message_bar(&self, contents: &mut String) {
        Editor::clear_row(contents);
        let message = if self.status_message.len() > self.screen_dimensions.cols
        {
            &self.status_message[0..self.screen_dimensions.cols]
        } else {
            &self.status_message
        };

        if !message.is_empty()
            && self.status_message_time.elapsed().as_secs() < 5
        {
            contents.push_str(message);
        }
    }

    fn set_status_message(&mut self, message: &str) {
        self.status_message = message.to_string();
        self.status_message_time = Instant::now();
    }

    fn refresh_screen(&mut self) {
        self.scroll();

        let mut contents = String::new();

        Editor::hide_cursor(&mut contents);
        Editor::reset_cursor(&mut contents);

        self.draw_rows(&mut contents);
        self.draw_status_bar(&mut contents);
        self.draw_message_bar(&mut contents);

        let cursor_screen_position = Position {
            x: self.cursor_render_x - self.text_offset.x,
            y: self.cursor_position.y - self.text_offset.y,
        };
        Editor::draw_cursor(&mut contents, &cursor_screen_position);

        Editor::show_cursor(&mut contents);

        print!("{}", contents);
        io::stdout().flush().unwrap();
    }

    fn reset_screen(&self) {
        let mut contents = String::new();

        Editor::clear_screen(&mut contents);
        Editor::reset_cursor(&mut contents);

        print!("{}", contents);
        io::stdout().flush().unwrap();
    }

    // *** Input ***

    fn prompt<F>(&mut self, prompt: &str, callback: F) -> Option<String>
    where
        F: Fn(&mut Editor, &str, Key) -> String,
    {
        let mut input = String::new();
        let mut message = String::new();
        loop {
            self.set_status_message(&format!(
                "{} {}",
                prompt.replace("{}", &input),
                &message
            ));
            self.refresh_screen();

            let key = self.read_key();
            match key {
                Key::Backspace | Key::Delete => {
                    input.pop();
                }
                Key::Esc => {
                    self.set_status_message("");
                    callback(self, &input, key);
                    return None;
                }
                Key::Enter => {
                    if !input.is_empty() {
                        self.set_status_message("");
                        callback(self, &input, key);
                        return Some(input);
                    }
                }
                Key::Char(c) => {
                    input.push(c);
                }
                _ => {}
            }
            message = callback(self, &input, key);
        }
    }

    fn read_key(&self) -> Key {
        match self.input.recv() {
            Ok(c) => {
                if c == '\x08' || c == '\x7f' {
                    Key::Backspace
                } else if c == '\r' {
                    Key::Enter
                } else if c == '\x1b' {
                    self.read_escape_sequence()
                } else if c.is_control() {
                    Key::Ctrl((c as u8 | 0b01100000) as char)
                } else {
                    Key::Char(c)
                }
            }
            Err(_) => panic!("Error reading from input channel"),
        }
    }

    fn read_escape_sequence(&self) -> Key {
        match self.input.try_recv() {
            Ok('[') => match self.input.try_recv() {
                Ok('A') => Key::Arrow(Arrow::Up),    // <esc>[A
                Ok('B') => Key::Arrow(Arrow::Down),  // <esc>[B
                Ok('C') => Key::Arrow(Arrow::Right), // <esc>[C
                Ok('D') => Key::Arrow(Arrow::Left),  // <esc>[D
                Ok('H') => Key::Home,                // <esc>[H
                Ok('F') => Key::End,                 // <esc>[F
                Ok(n @ '0'..='9') => match self.input.try_recv() {
                    Ok('~') => match n {
                        // Match on the number before the tilde.
                        '1' | '7' => Key::Home, // <esc>[1~ or <esc>[7~
                        '4' | '8' => Key::End,  // <esc>[4~ or <esc>[8~
                        '3' => Key::Delete,     // <esc>[3~
                        '5' => Key::PageUp,     // <esc>[5~
                        '6' => Key::PageDown,   // <esc>[6~
                        _ => Key::Esc,
                    },
                    // Ignore all bytes after the esc.
                    Ok(_) | Err(TryRecvError::Empty) => Key::Esc,
                    Err(TryRecvError::Disconnected) => {
                        panic!("Input channel disconnected")
                    }
                },
                // Ignore all bytes after the esc.
                Ok(_) | Err(TryRecvError::Empty) => Key::Esc,
                Err(TryRecvError::Disconnected) => {
                    panic!("Input channel disconnected")
                }
            },
            Ok('O') => match self.input.try_recv() {
                Ok('H') => Key::Home, // <esc>OH
                Ok('F') => Key::End,  // <esc>OF
                // Ignore all bytes after the esc.
                Ok(_) | Err(TryRecvError::Empty) => Key::Esc,
                Err(TryRecvError::Disconnected) => {
                    panic!("Input channel disconnected")
                }
            },
            // Ignore the byte after the esc if there is one.
            Ok(_) | Err(TryRecvError::Empty) => Key::Esc,
            Err(TryRecvError::Disconnected) => {
                panic!("Input channel disconnected")
            }
        }
    }
    fn move_cursor(&mut self, arrow: Arrow) {
        match arrow {
            Arrow::Up => {
                if self.cursor_position.y > 0 {
                    self.cursor_position.y -= 1;
                    self.cursor_position.x = self.get_char_index();
                }
            }
            Arrow::Left => {
                if self.cursor_position.x > 0 {
                    self.cursor_position.x -= 1
                } else if self.cursor_position.y > 0 {
                    self.cursor_position.y -= 1;
                    self.cursor_position.x =
                        self.get_current_row().unwrap().chars.chars().count();
                }
            }
            Arrow::Down => {
                if self.cursor_position.y < self.rows.len() {
                    self.cursor_position.y += 1;
                    self.cursor_position.x = self.get_char_index();
                }
            }
            Arrow::Right => {
                if let Some(row) = self.get_current_row() {
                    #[allow(clippy::comparison_chain)]
                    if self.cursor_position.x < row.chars.chars().count() {
                        self.cursor_position.x += 1
                    } else if self.cursor_position.x
                        == row.chars.chars().count()
                    {
                        self.cursor_position.y += 1;
                        self.cursor_position.x = 0;
                    }
                }
            }
        };

        let row_length = if let Some(row) = self.get_current_row() {
            row.chars.chars().count()
        } else {
            0
        };

        // Move the cursor to the end of the line if it is past the end.
        if self.cursor_position.x > row_length {
            self.cursor_position.x = row_length;
        }
    }

    fn scroll(&mut self) {
        // Update which part of the file we're looking at based on the new
        // position of the cursor.
        self.cursor_render_x = self.get_render_index();

        if self.cursor_position.y < self.text_offset.y {
            self.text_offset.y = self.cursor_position.y;
        }

        if self.cursor_position.y
            >= self.text_offset.y + self.screen_dimensions.rows
        {
            self.text_offset.y =
                self.cursor_position.y - self.screen_dimensions.rows + 1;
        }

        if self.cursor_render_x < self.text_offset.x {
            self.text_offset.x = self.cursor_render_x;
        }

        if self.cursor_render_x
            >= self.text_offset.x + self.screen_dimensions.cols
        {
            self.text_offset.x =
                self.cursor_render_x - self.screen_dimensions.cols + 1;
        }
    }

    fn process_keypress(&mut self) -> KeypressResult {
        let key = self.read_key();

        let mut result = KeypressResult::Continue;

        match key {
            Key::Enter => {
                self.insert_newline();
            }
            Key::Ctrl('q') => {
                if self.dirty && self.quit_times > 0 {
                    self.set_status_message(&format!(
                        "WARNING!!! File has unsaved changes. \
                         Press Ctrl-Q {} more times to quit.",
                        self.quit_times
                    ));
                    self.quit_times -= 1;
                    return result;
                } else {
                    result = KeypressResult::Terminate;
                }
            }
            Key::Ctrl('s') => {
                self.save();
            }
            Key::Ctrl('r') => {
                self.find();
            }
            Key::Arrow(arrow) => {
                self.move_cursor(arrow);
            }
            key @ Key::PageUp | key @ Key::PageDown => {
                match key {
                    Key::PageUp => self.cursor_position.y = self.text_offset.y,
                    Key::PageDown => {
                        self.cursor_position.y = self.text_offset.y
                            + self.screen_dimensions.rows
                            - 1;
                        if self.cursor_position.y > self.rows.len() {
                            self.cursor_position.y = self.rows.len();
                        }
                    }
                    _ => {}
                }

                for _ in 0..self.screen_dimensions.rows - 1 {
                    self.move_cursor(if let Key::PageUp = key {
                        Arrow::Up
                    } else {
                        Arrow::Down
                    });
                }
            }
            Key::Home => {
                self.cursor_position.x = 0;
            }
            Key::End => {
                if let Some(row) = self.get_current_row() {
                    self.cursor_position.x = row.chars.chars().count();
                }
            }
            Key::Backspace => {
                self.delete_char();
            }
            Key::Delete => {
                self.move_cursor(Arrow::Right);
                self.delete_char();
            }
            // Ignore these keys.
            Key::Ctrl('l') | Key::Esc => {}
            Key::Char(c) => {
                self.insert_char(c);
            }
            Key::Ctrl(c) => {
                self.insert_char((c as u8 & 0b10011111) as char);
            }
        };

        self.quit_times = QUIT_TIMES;
        result
    }

    fn render_loop(&mut self) {
        loop {
            self.refresh_screen();
            if let KeypressResult::Terminate = self.process_keypress() {
                break;
            }
        }

        self.reset_screen();
    }
}

/*** init ***/

fn enable_raw_mode() -> Termios {
    let stdin_raw_fd = io::stdin().as_raw_fd();
    let orig_termios =
        termios::tcgetattr(stdin_raw_fd).expect("Error in tcgetattr");

    let mut termios = orig_termios.clone();
    termios.input_flags &= !(InputFlags::BRKINT
        | InputFlags::ICRNL
        | InputFlags::INPCK
        | InputFlags::ISTRIP
        | InputFlags::IXON);
    termios.output_flags &= !(OutputFlags::OPOST);
    termios.control_flags |= ControlFlags::CS8;
    termios.local_flags &= !(LocalFlags::ECHO
        | LocalFlags::ICANON
        | LocalFlags::IEXTEN
        | LocalFlags::ISIG);
    // Rust always blocks when reading from stdin.
    // termios.c_cc[VMIN] = 0;
    // termios.c_cc[VTIME] = 1;
    termios::tcsetattr(stdin_raw_fd, SetArg::TCSAFLUSH, &termios)
        .expect("Error in tcsetattr");

    orig_termios
}

fn disable_raw_mode(orig_termios: &mut Termios) {
    let stdin_raw_fd = io::stdin().as_raw_fd();
    termios::tcsetattr(stdin_raw_fd, SetArg::TCSAFLUSH, orig_termios)
        .expect("Error in tcsetattr");
}

struct TerminalRestorer {
    orig_termios: Termios,
}

impl Drop for TerminalRestorer {
    fn drop(&mut self) {
        disable_raw_mode(&mut self.orig_termios);
    }
}

fn main() {
    // Enabling raw mode and saving current terminal options.
    let orig_termios = enable_raw_mode();
    // Restore the original terminal options when this struct is dropped.
    // This ensures the original options are restored even if we panic.
    let _terminal_restorer = TerminalRestorer { orig_termios };

    let mut editor = Editor::new();

    let mut args = env::args();
    if args.len() >= 2 {
        editor.open(&args.nth(1).unwrap());
    }

    editor.set_status_message(
        "HELP: Ctrl-S = Save | Ctrl-F = Find | Ctrl-Q = Quit",
    );

    editor.render_loop();
}
