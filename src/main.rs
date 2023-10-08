use nix::sys::termios::{self, ControlFlags, InputFlags, LocalFlags, OutputFlags, SetArg, Termios};
use regex::Regex;
use std::cmp;
use std::env;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::mem;
use std::os::unix::io::AsRawFd;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthChar;

const VERSION: &str = "0.0.1";

// TODO: get config from config file.
const TAB_STOP: usize = 8;
const MAX_STATUS_FILENAME_LENGTH: usize = 20;
const QUIT_TIMES: u8 = 3;
const RENDER_WHITESPACE: bool = false;

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
    let (width, height) =
        term_size::dimensions_stdin().expect("Failed to get terminal dimensions.");
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

#[allow(dead_code)]
#[derive(Copy, Clone, PartialEq)]
enum Color {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    BrightBlack,
    BrightRed,
    BrightGreen,
    BrightYellow,
    BrightBlue,
    BrightMagenta,
    BrightCyan,
    BrightWhite,
    Default,
}

#[derive(Copy, Clone, PartialEq)]
enum Highlight {
    Normal,
    Number,
    String,
    Comment,
    MultilineComment,
    Keyword1,
    Keyword2,
    Match,
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

// *** Filetypes ***

const HIGHLIGHT_NUMBERS: u32 = 1 << 0;
const HIGHLIGHT_STRINGS: u32 = 1 << 1;

struct Filetype {
    name: &'static str,
    filename_patterns: &'static [&'static str],
    singleline_comment_start: &'static str,
    multiline_comment_start: &'static str,
    multiline_comment_end: &'static str,
    keywords1: &'static [&'static str],
    keywords2: &'static [&'static str],
    flags: u32,
}
const FILETYPES: [Filetype; 3] = [
    Filetype {
        name: "c",
        filename_patterns: &[".c", ".h", ".cpp"],
        singleline_comment_start: "//",
        multiline_comment_start: "/*",
        multiline_comment_end: "*/",
        keywords1: &[
            "switch", "if", "while", "for", "break", "continue", "return", "else", "struct",
            "union", "typedef", "static", "enum", "class", "case",
        ],
        keywords2: &[
            "int", "long", "double", "float", "char", "unsigned", "signed", "void",
        ],
        flags: HIGHLIGHT_NUMBERS | HIGHLIGHT_STRINGS,
    },
    Filetype {
        name: "rust",
        filename_patterns: &[".rs"],
        singleline_comment_start: "//",
        multiline_comment_start: "/*",
        multiline_comment_end: "*/",
        keywords1: &[
            "if", "while", "for", "loop", "break", "continue", "return", "else", "match", "mut",
            "fn", "move", "in", "as", "impl", "where", "use",
        ],
        keywords2: &["let", "struct", "const", "enum"],
        flags: HIGHLIGHT_NUMBERS | HIGHLIGHT_STRINGS,
    },
    Filetype {
        name: "python",
        filename_patterns: &[".py"],
        singleline_comment_start: "#",
        multiline_comment_start: "",
        multiline_comment_end: "",
        keywords1: &[
            "import", "from", "yield", "return", "if", "elif", "else", "while", "for", "in", "is",
            "not", "and", "or",
        ],
        keywords2: &[
            "def",
            "str",
            "set",
            "dict",
            "list",
            "float",
            "int",
            "bool",
            "print",
            "enumerate",
            "len",
            "input",
            "reversed",
        ],
        flags: HIGHLIGHT_NUMBERS | HIGHLIGHT_STRINGS,
    },
];

fn is_separator(c: char) -> bool {
    c.is_whitespace() || "&,.()+-/*=~%<>[]; ".contains(c)
}

struct Row {
    chars: String,
    render: String,
    highlight: Vec<Highlight>,
    continue_multiline_comment: bool,
    continue_multiline_string: Option<char>,
}

impl Row {
    fn zip(&self) -> Vec<(char, usize, char, Highlight)> {
        let mut result = Vec::new();

        let mut render_iter = self.render.chars();
        let mut render_length = 0;
        let mut highlight_iter = self.highlight.iter();

        for (i, c) in self.chars.chars().enumerate() {
            if c == '\t' {
                let mut tab_size = TAB_STOP - (render_length % TAB_STOP);
                while tab_size > 0 {
                    result.push((
                        c,
                        i,
                        render_iter.next().unwrap(),
                        *highlight_iter.next().unwrap(),
                    ));
                    render_length += 1;
                    tab_size -= 1;
                }
            } else if c.is_control() {
                result.push((
                    c,
                    i,
                    render_iter.next().unwrap(),
                    *highlight_iter.next().unwrap(),
                ));
                render_length += 1;
            } else {
                result.push((
                    c,
                    i,
                    render_iter.next().unwrap(),
                    *highlight_iter.next().unwrap(),
                ));
                render_length += 1;
                for _ in 0..UnicodeWidthChar::width(c).unwrap_or(1) - 1 {
                    render_length += 1;
                }
            }
        }
        result
    }
}

struct Editor {
    screen_dimensions: Dimensions,
    cursor_position: Position,
    input: Receiver<char>,
    text_offset: Position,
    rows: Vec<Row>,
    filename: Option<String>,
    filetype: Option<&'static Filetype>,
    status_message: String,
    status_message_time: Instant,
    dirty: bool,
    quit_times: u8,
    matches: Vec<usize>,
    match_index: usize,
    saved_highlight: Vec<Highlight>,
    saved_highlight_index: usize,
}

impl Editor {
    fn new() -> Editor {
        let mut screen_dimensions = get_window_size();
        screen_dimensions.rows -= 2; // Make room for status bar and message.

        Editor {
            screen_dimensions,
            cursor_position: Position { x: 0, y: 0 },
            input: spawn_stdin_channel(),
            text_offset: Position { x: 0, y: 0 },
            rows: Vec::new(),
            filename: None,
            filetype: None,
            status_message: String::new(),
            status_message_time: Instant::now(),
            dirty: false,
            quit_times: QUIT_TIMES,
            matches: Vec::new(),
            match_index: 0,
            saved_highlight: Vec::new(),
            saved_highlight_index: 0,
        }
    }

    fn highlight_to_color(highlight: Highlight) -> Color {
        match highlight {
            Highlight::Number => Color::Magenta,
            Highlight::String => Color::Yellow,
            Highlight::Comment | Highlight::MultilineComment => Color::BrightBlack,
            Highlight::Keyword1 => Color::Red,
            Highlight::Keyword2 => Color::Cyan,
            Highlight::Match => Color::Blue,
            _ => Color::White,
        }
    }

    fn detect_filetype(&mut self) {
        match &self.filename {
            Some(name) => {
                for filetype in &FILETYPES {
                    for pattern in filetype.filename_patterns {
                        if (pattern.starts_with('.') && name.ends_with(pattern))
                            || (!pattern.starts_with('.') && name.contains(pattern))
                        {
                            self.filetype = Some(filetype);
                            for y in 0..self.rows.len() {
                                self.update_row_highlight(y);
                            }
                            return;
                        }
                    }
                }
            }
            None => {}
        }
    }

    // *** Row Operations ***

    fn update_row_highlight(&mut self, y: usize) {
        if y >= self.rows.len() {
            return;
        }
        let (first, last) = self.rows.split_at_mut(y);
        let row = &mut last[0];

        row.highlight.clear();
        let mut chars = row.render.char_indices().enumerate();
        let line_length = row.render.chars().count();

        let singleline_comment_start = if let Some(f) = self.filetype {
            f.singleline_comment_start
        } else {
            ""
        };
        let multiline_comment_start = if let Some(f) = self.filetype {
            f.multiline_comment_start
        } else {
            ""
        };
        let multiline_comment_end = if let Some(f) = self.filetype {
            f.multiline_comment_end
        } else {
            ""
        };

        let mut prev_separator = true;
        let mut in_singleline_comment = false;
        let mut in_multiline_comment = y != 0 && first[first.len() - 1].continue_multiline_comment;

        let mut quote = if y == 0 || in_multiline_comment {
            None
        } else {
            first[first.len() - 1].continue_multiline_string
        };

        loop {
            let prev_highlight = *row.highlight.last().unwrap_or(&Highlight::Normal);
            let next = chars.next();
            if let Some((i, (byte_index, c))) = next {
                // No syntax highlighting.
                if self.filetype.is_none() {
                    row.highlight.push(Highlight::Normal);
                    continue;
                }

                // Continuing a single-line comment.
                if in_singleline_comment {
                    row.highlight.push(Highlight::Comment);
                    continue;
                }

                // Starting a single-line comment.
                if !singleline_comment_start.is_empty()
                    && quote.is_none()
                    && !in_multiline_comment
                    && row.render[byte_index..].starts_with(singleline_comment_start)
                {
                    in_singleline_comment = true;
                    row.highlight.push(Highlight::Comment);
                    continue;
                }

                // Multi-line comments.
                if !multiline_comment_start.is_empty() && quote.is_none() {
                    if in_multiline_comment {
                        if row.render[byte_index..].starts_with(multiline_comment_end) {
                            row.highlight.push(Highlight::MultilineComment);
                            for _ in 0..multiline_comment_end.len() - 1 {
                                chars.next();
                                row.highlight.push(Highlight::MultilineComment);
                            }
                            in_multiline_comment = false;
                        } else {
                            row.highlight.push(Highlight::MultilineComment);
                        }
                        continue;
                    } else if row.render[byte_index..].starts_with(multiline_comment_start) {
                        row.highlight.push(Highlight::MultilineComment);
                        for _ in 0..multiline_comment_start.len() - 1 {
                            chars.next();
                            row.highlight.push(Highlight::MultilineComment);
                        }
                        in_multiline_comment = true;
                        continue;
                    }
                }

                // Strings.
                if self.filetype.unwrap().flags & HIGHLIGHT_STRINGS != 0 {
                    match quote {
                        Some(q) => {
                            // In a string.
                            row.highlight.push(Highlight::String);
                            if c == '\\' {
                                if i < line_length - 1 {
                                    row.highlight.push(Highlight::String);
                                    chars.next();
                                    if i == line_length - 2 {
                                        quote = None;
                                    }
                                }
                            } else if c == q {
                                // String ends.
                                quote = None;
                                prev_separator = true;
                            } else if i == line_length - 1 {
                                quote = None;
                            }
                            continue;
                        }
                        None => {
                            // Not in a string.
                            if c == '"' || c == '\'' {
                                // String starts.
                                quote = Some(c);
                                row.highlight.push(Highlight::String);
                                if i == line_length - 1 {
                                    quote = None;
                                }
                                continue;
                            }
                        }
                    }
                }

                // Numbers.
                if self.filetype.unwrap().flags & HIGHLIGHT_NUMBERS != 0
                    && ((c.is_digit(10) && (prev_separator || prev_highlight == Highlight::Number))
                        || (c == '.' && prev_highlight == Highlight::Number))
                {
                    row.highlight.push(Highlight::Number);
                    prev_separator = false;
                    continue;
                }

                // Keywords.
                if prev_separator {
                    let mut found_keyword = false;
                    'outer: for (keywords, highlight) in [
                        (self.filetype.unwrap().keywords1, Highlight::Keyword1),
                        (self.filetype.unwrap().keywords2, Highlight::Keyword2),
                    ] {
                        for keyword in keywords {
                            if row.render[byte_index..].starts_with(keyword)
                                && is_separator(
                                    row.render[byte_index + keyword.len()..]
                                        .chars()
                                        .next()
                                        .unwrap_or(' '),
                                )
                            {
                                row.highlight.push(highlight);
                                for _ in 0..keyword.len() - 1 {
                                    chars.next();
                                    row.highlight.push(highlight);
                                }
                                found_keyword = true;
                                break 'outer;
                            }
                        }
                    }
                    if found_keyword {
                        prev_separator = false;
                        continue;
                    }
                }

                row.highlight.push(Highlight::Normal);
                prev_separator = is_separator(c);
            } else {
                break;
            }
        }

        // Check whether we need to update the syntax of following lines.
        // eg. we could start a multiline comment on this line which could
        // comment out the rest of the file.
        let changed = row.continue_multiline_string != quote
            || row.continue_multiline_comment != in_multiline_comment;
        row.continue_multiline_comment = in_multiline_comment;
        row.continue_multiline_string = quote;
        if changed {
            self.update_row_highlight(y + 1);
        }
    }

    fn update_row_render(&mut self, y: usize) {
        if y >= self.rows.len() {
            return;
        }
        let row = &mut self.rows[y];
        row.render.clear();

        let mut render_length = 0;

        for c in row.chars.chars() {
            if c == '\t' {
                let mut tab_size = TAB_STOP - (render_length % TAB_STOP);
                while tab_size > 0 {
                    row.render.push(' ');
                    render_length += 1;
                    tab_size -= 1;
                }
            } else if c.is_control() {
                row.render.push(c);
                render_length += 1;
            } else {
                row.render.push(c);
                render_length += 1;
                for _ in 0..UnicodeWidthChar::width(c).unwrap_or(1) - 1 {
                    render_length += 1;
                }
            }
        }
    }

    fn update_row(&mut self, y: usize) {
        self.update_row_render(y);
        self.update_row_highlight(y);
    }

    fn insert_char_in_row(&mut self, y: usize, mut index: usize, c: char) {
        if y >= self.rows.len() {
            return;
        }
        let row = &mut self.rows[y];
        let count = row.chars.chars().count();
        if index > count {
            index = count;
        }

        let mut new_chars = String::new();

        if index == count {
            row.chars.push(c);
        } else {
            for (i, char) in row.chars.chars().enumerate() {
                if i == index {
                    new_chars.push(c);
                }
                new_chars.push(char);
            }

            row.chars = new_chars;
        }

        self.update_row(y);
    }

    fn append_string_to_row(&mut self, y: usize, s: &str) {
        if y >= self.rows.len() {
            return;
        }
        let row = &mut self.rows[y];
        row.chars.push_str(s);
        self.update_row(y);
    }

    fn delete_char_in_row(&mut self, y: usize, index: usize) {
        if y >= self.rows.len() {
            return;
        }
        let row = &mut self.rows[y];
        let count = row.chars.chars().count();
        if index >= count {
            return;
        }

        let mut new_chars = String::new();

        for (i, char) in row.chars.chars().enumerate() {
            if i != index {
                new_chars.push(char);
            }
        }

        row.chars = new_chars;
        self.update_row(y);
    }

    fn insert_row(&mut self, index: usize, chars: &str) {
        if index > self.rows.len() {
            return;
        }

        let row = Row {
            chars: chars.to_string(),
            render: String::new(),
            highlight: Vec::new(),
            continue_multiline_comment: false,
            continue_multiline_string: None,
        };
        self.rows.insert(index, row);
        self.update_row(index);
        self.dirty = true;
    }

    // TODO: get_screen_index, screen_index_to_char_index and get_render_index
    // are pretty similar.
    fn get_screen_index(&self, x: usize, y: usize) -> usize {
        if y >= self.rows.len() || x == 0 {
            return 0;
        }

        let mut screen_index = 0;

        let row = &self.rows[y];

        for c in row.chars.chars().take(x) {
            if c == '\t' {
                screen_index += (TAB_STOP - 1) - (screen_index % TAB_STOP) + 1;
            } else if c.is_control() {
                screen_index += 1;
            } else {
                screen_index += UnicodeWidthChar::width(c).unwrap_or(0);
            }
        }
        screen_index
    }

    fn get_current_screen_index(&self) -> usize {
        self.get_screen_index(self.cursor_position.x, self.cursor_position.y)
    }

    fn screen_index_to_char_index(screen_index: usize, row: Option<&Row>) -> usize {
        if row.is_none() || screen_index == 0 {
            return 0;
        }

        let mut char_index = 0;
        let mut i = 0;

        for c in row.unwrap().chars.chars() {
            if c == '\t' {
                i += (TAB_STOP - 1) - (i % TAB_STOP) + 1;
            } else if c.is_control() {
                i += 1;
            } else {
                i += UnicodeWidthChar::width(c).unwrap_or(0);
            }

            char_index += 1;
            if i >= screen_index {
                return char_index;
            }
        }
        char_index
    }

    fn get_render_index(&self, x: usize, y: usize) -> usize {
        if y >= self.rows.len() || x == 0 {
            return 0;
        }

        let mut render_index = 0;

        let row = &self.rows[y];

        for c in row.chars.chars().take(x) {
            if c == '\t' {
                render_index += (TAB_STOP - 1) - (render_index % TAB_STOP) + 1;
            } else {
                render_index += 1;
            }
        }
        render_index
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

        self.insert_char_in_row(self.cursor_position.y, self.cursor_position.x, c);
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
            self.delete_char_in_row(self.cursor_position.y, self.cursor_position.x - 1);
            self.cursor_position.x -= 1;
            self.dirty = true;
        } else {
            self.cursor_position.x = self.rows[self.cursor_position.y - 1].chars.chars().count();
            let chars = mem::take(&mut self.rows[self.cursor_position.y].chars);
            self.append_string_to_row(self.cursor_position.y - 1, &chars);
            self.delete_row(self.cursor_position.y);
            self.cursor_position.y -= 1;
        }
    }

    fn insert_newline(&mut self) {
        if self.cursor_position.x == 0 {
            self.insert_row(self.cursor_position.y, "");
        } else {
            let row = &mut self.rows[self.cursor_position.y];
            let split_at = row
                .chars
                .char_indices()
                .nth(self.cursor_position.x)
                .unwrap_or((row.chars.len(), 'a'))
                .0;
            let new_row_contents = row.chars.split_at(split_at).1.to_string();

            row.chars.truncate(split_at);

            self.insert_row(self.cursor_position.y + 1, &new_row_contents);
            self.update_row(self.cursor_position.y);
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
        let f = match File::open(filename) {
            Ok(f) => f,
            Err(e) => {
                if e.kind() == std::io::ErrorKind::NotFound {
                    File::create(filename).expect("Unable to create file.");
                    File::open(filename).expect("Unable to open file.")
                } else {
                    panic!("Unable to open file.")
                }
            }
        };
        let reader = BufReader::new(f);
        let lines = reader.lines();

        for l in lines {
            self.insert_row(self.rows.len(), &l.expect("Error reading file"));
        }

        self.filename = Some(filename.to_string());
        self.detect_filetype();
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
            self.filename = self.prompt("Save as: {} (ESC to cancel)", |_, _, _| String::new());
            if self.filename.is_none() {
                self.set_status_message("Save aborted");
                return;
            }
            self.detect_filetype();
        }

        match File::create(self.filename.as_ref().unwrap()) {
            Ok(mut file) => {
                let file_contents = self.rows_to_string();
                match file.write_all(file_contents.as_bytes()) {
                    Ok(_) => {
                        self.set_status_message(&format!("{} bytes written", file_contents.len()));
                        self.dirty = false;
                    }
                    // An error here means the file contents are lost. Oh well.
                    Err(error) => self.set_status_message(&format!("Save failed: {}", error)),
                }
            }
            Err(error) => self.set_status_message(&format!("Save failed: {:?}", error)),
        }
    }

    // *** Find ***

    fn find_callback(&mut self, query: &str, key: Key) -> String {
        if !self.saved_highlight.is_empty() {
            std::mem::swap(
                &mut self.saved_highlight,
                &mut self.rows[self.saved_highlight_index].highlight,
            );
            self.saved_highlight.clear();
            self.saved_highlight_index = 0;
        }

        if query.is_empty() {
            return String::new();
        }

        let regex = match Regex::new(query) {
            Ok(re) => re,
            _ => return ": Invalid regex".to_string(),
        };

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
                self.match_index = if self.match_index == self.matches.len() - 1 {
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
        // TODO: Only finds the first match in each line.
        let row_index = regex.find(&row.chars).unwrap();
        self.cursor_position.y = self.matches[self.match_index];
        self.text_offset.y = self.matches[self.match_index];
        // Translate the byte offsets into char offsets.
        let mut start = 0;
        let mut end = 0;
        for (i, (byte_offset, _)) in row.chars.char_indices().enumerate() {
            if byte_offset == row_index.start() {
                start = i;
            }
            if byte_offset == row_index.end() - 1 {
                end = i;
                break;
            }
        }

        self.cursor_position.x = start;

        // Highlight the match.
        self.saved_highlight_index = self.matches[self.match_index];
        self.saved_highlight = row.highlight.clone();
        let render_start = self.get_render_index(start, self.cursor_position.y);
        let render_end = self.get_render_index(end, self.cursor_position.y);

        let row = &mut self.rows[self.matches[self.match_index]];

        for i in render_start..render_end + 1 {
            row.highlight[i] = Highlight::Match;
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
        let s = format!("\x1b[{};{}H", cursor_position.y + 1, cursor_position.x + 1);
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

    fn set_color(contents: &mut String, color: Color) {
        let color_code = match color {
            Color::Black => "0;30",
            Color::Red => "0;31",
            Color::Green => "0;32",
            Color::Yellow => "0;33",
            Color::Blue => "0;34",
            Color::Magenta => "0;35",
            Color::Cyan => "0;36",
            Color::White => "0;37",
            Color::BrightBlack => "1;30",
            Color::BrightRed => "1;31",
            Color::BrightGreen => "1;32",
            Color::BrightYellow => "1;33",
            Color::BrightBlue => "1;34",
            Color::BrightMagenta => "1;35",
            Color::BrightCyan => "1;36",
            Color::BrightWhite => "1;37",
            Color::Default => "0;39",
        };
        contents.push_str(&format!("\x1b[{}m", color_code));
    }

    fn invert_colors(contents: &mut String) {
        contents.push_str("\x1b[7m");
    }

    fn clear_formatting(contents: &mut String) {
        contents.push_str("\x1b[m");
    }

    fn draw_rows(&self, contents: &mut String) {
        let line_number_padding = format!("{}", self.rows.len()).len();
        for y in 0..self.screen_dimensions.rows {
            let mut filled_line = false;
            let file_row = y + self.text_offset.y;
            if file_row >= self.rows.len() {
                if self.rows.is_empty() && y == self.screen_dimensions.rows / 3 {
                    let welcome_message = format!("Kilo editor -- version {}", VERSION);
                    let message_length =
                        cmp::min(welcome_message.len(), self.screen_dimensions.cols - 1);

                    let mut padding = (self.screen_dimensions.cols - message_length) / 2;
                    if padding > 0 {
                        Editor::set_color(contents, Color::Blue);
                        contents.push('~');
                        Editor::set_color(contents, Color::Default);
                        padding -= 1;
                    }

                    for _ in 0..padding {
                        contents.push(' ');
                    }

                    contents.push_str(&welcome_message[..message_length]);
                } else {
                    Editor::set_color(contents, Color::Blue);
                    contents.push('~');
                    Editor::set_color(contents, Color::Default);
                }
            } else {
                Editor::set_color(contents, Color::BrightBlack);
                contents.push_str(&format!(
                    "{:>width$} ",
                    file_row + 1,
                    width = line_number_padding
                ));
                Editor::set_color(contents, Color::Default);

                let line_length = self.rows[file_row]
                    .render
                    .chars()
                    .map(|c| {
                        if c.is_control() {
                            1
                        } else {
                            UnicodeWidthChar::width(c).unwrap_or(0)
                        }
                    })
                    .sum();
                // Check if any of this line is visible.
                if self.text_offset.x < line_length {
                    let mut displayed_length = line_length - self.text_offset.x;
                    // Cap the displayed length to the length of the screen.
                    if displayed_length >= self.screen_dimensions.cols - (line_number_padding + 1) {
                        displayed_length = self.screen_dimensions.cols - (line_number_padding + 1);
                        filled_line = true;
                    }
                    // Start displaying the line at the text offset.
                    let row = &self.rows[file_row];
                    let start_index = self.text_offset.x;
                    let mut current_color = Color::Default;
                    let mut screen_index = 0;
                    let mut prev_width = 0;

                    let mut zip = row.zip().into_iter().peekable();

                    loop {
                        let next = zip.next();
                        if let Some((char, char_index, render, highlight)) = next {
                            let curr_width = if char.is_control() {
                                1
                            } else {
                                UnicodeWidthChar::width(char).unwrap_or(0)
                            };
                            if screen_index >= start_index
                                && screen_index < start_index + displayed_length
                            {
                                if prev_width > 1
                                    && screen_index > start_index
                                    && screen_index - prev_width < start_index
                                {
                                    // There's a cut off wide character at the start
                                    // of the row.
                                    Editor::set_color(contents, Color::Blue);
                                    contents.push('<');
                                    Editor::set_color(contents, current_color);
                                } else if curr_width > 1
                                    && screen_index + curr_width > start_index + displayed_length
                                {
                                    // There's a cut off wide character at the end
                                    // of the row.
                                    Editor::set_color(contents, Color::Blue);
                                    contents.push('>');
                                    Editor::set_color(contents, current_color);
                                    prev_width = curr_width;
                                    screen_index += curr_width;
                                    continue;
                                }

                                if RENDER_WHITESPACE && char == ' ' {
                                    Editor::set_color(contents, Color::BrightBlack);
                                    contents.push('∙');
                                    Editor::set_color(contents, current_color);
                                } else if RENDER_WHITESPACE && char == '\t' {
                                    Editor::set_color(contents, Color::BrightBlack);
                                    contents.push('⇀');
                                    while zip.peek().is_some()
                                        && zip.peek().unwrap().1 == char_index
                                    {
                                        zip.next();
                                        contents.push(' ');
                                    }
                                    Editor::set_color(contents, current_color);
                                } else if render.is_control() {
                                    Editor::invert_colors(contents);
                                    contents.push(if char as u8 <= 26 {
                                        (char as u8 | !0b10111111) as char
                                    } else {
                                        '?'
                                    });
                                    Editor::clear_formatting(contents);
                                    Editor::set_color(contents, current_color);
                                } else if let Highlight::Normal = highlight {
                                    if current_color != Color::Default {
                                        Editor::set_color(contents, Color::Default);
                                        current_color = Color::Default;
                                    }
                                    contents.push(render);
                                } else {
                                    let color = Editor::highlight_to_color(highlight);
                                    if current_color != color {
                                        Editor::set_color(contents, color);
                                        current_color = color;
                                    }
                                    contents.push(render);
                                }
                            }
                            prev_width = curr_width;
                            screen_index += curr_width;
                        } else {
                            break;
                        }
                    }
                    Editor::set_color(contents, Color::Default);
                }
            }
            if !filled_line {
                Editor::clear_row(contents);
            }

            contents.push_str("\r\n");
        }
    }

    fn draw_status_bar(&self, contents: &mut String) {
        Editor::invert_colors(contents);

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
            "{} | {}:{} ",
            if self.filetype.is_none() {
                "no ft"
            } else {
                self.filetype.unwrap().name
            },
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

        Editor::clear_formatting(contents);
        contents.push_str("\r\n");
    }

    fn draw_message_bar(&self, contents: &mut String) {
        Editor::clear_row(contents);
        let message = if self.status_message.len() > self.screen_dimensions.cols {
            &self.status_message[0..self.screen_dimensions.cols]
        } else {
            &self.status_message
        };

        if !message.is_empty() && self.status_message_time.elapsed().as_secs() < 5 {
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

        let line_number_space = format!("{}", self.rows.len()).len() + 1;

        let cursor_screen_position = Position {
            x: self.get_current_screen_index() - self.text_offset.x + line_number_space,
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
            self.set_status_message(&format!("{} {}", prompt.replace("{}", &input), &message));
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
        match self.input.recv_timeout(Duration::from_millis(100)) {
            Ok('[') => match self.input.try_recv() {
                Ok('A') => Key::Arrow(Arrow::Up),    // <esc>[A
                Ok('B') => Key::Arrow(Arrow::Down),  // <esc>[B
                Ok('C') => Key::Arrow(Arrow::Right), // <esc>[C
                Ok('D') => Key::Arrow(Arrow::Left),  // <esc>[D
                Ok('H') => Key::Home,                // <esc>[H
                Ok('F') => Key::End,                 // <esc>[F
                Ok(n @ '0'..='9') => {
                    match self.input.recv_timeout(Duration::from_millis(100)) {
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
                        Ok(_) | Err(RecvTimeoutError::Timeout) => Key::Esc,
                        Err(RecvTimeoutError::Disconnected) => {
                            panic!("Input channel disconnected")
                        }
                    }
                }
                // Ignore all bytes after the esc.
                Ok(_) | Err(TryRecvError::Empty) => Key::Esc,
                Err(TryRecvError::Disconnected) => {
                    panic!("Input channel disconnected")
                }
            },
            Ok('O') => {
                match self.input.recv_timeout(Duration::from_millis(100)) {
                    Ok('H') => Key::Home, // <esc>OH
                    Ok('F') => Key::End,  // <esc>OF
                    // Ignore all bytes after the esc.
                    Ok(_) | Err(RecvTimeoutError::Timeout) => Key::Esc,
                    Err(RecvTimeoutError::Disconnected) => {
                        panic!("Input channel disconnected")
                    }
                }
            }
            // Ignore the byte after the esc if there is one.
            Ok(_) | Err(RecvTimeoutError::Timeout) => Key::Esc,
            Err(RecvTimeoutError::Disconnected) => {
                panic!("Input channel disconnected")
            }
        }
    }
    fn move_cursor(&mut self, arrow: Arrow) {
        match arrow {
            Arrow::Up => {
                if self.cursor_position.y > 0 {
                    let screen_index = self.get_current_screen_index();
                    self.cursor_position.y -= 1;
                    self.cursor_position.x =
                        Editor::screen_index_to_char_index(screen_index, self.get_current_row());
                }
            }
            Arrow::Left => {
                if self.cursor_position.x > 0 {
                    self.cursor_position.x -= 1
                } else if self.cursor_position.y > 0 {
                    self.cursor_position.y -= 1;
                    self.cursor_position.x = self.get_current_row().unwrap().chars.chars().count();
                }
            }
            Arrow::Down => {
                if self.cursor_position.y < self.rows.len() {
                    let screen_index = self.get_current_screen_index();
                    self.cursor_position.y += 1;
                    self.cursor_position.x =
                        Editor::screen_index_to_char_index(screen_index, self.get_current_row());
                }
            }
            Arrow::Right => {
                if let Some(row) = self.get_current_row() {
                    #[allow(clippy::comparison_chain)]
                    if self.cursor_position.x < row.chars.chars().count() {
                        self.cursor_position.x += 1
                    } else if self.cursor_position.x == row.chars.chars().count() {
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
        let screen_x = self.get_current_screen_index();

        if self.cursor_position.y < self.text_offset.y {
            self.text_offset.y = self.cursor_position.y;
        }

        if self.cursor_position.y >= self.text_offset.y + self.screen_dimensions.rows {
            self.text_offset.y = self.cursor_position.y - self.screen_dimensions.rows + 1;
        }

        if screen_x < self.text_offset.x {
            self.text_offset.x = screen_x;
        }

        if screen_x >= self.text_offset.x + self.screen_dimensions.cols {
            self.text_offset.x = screen_x - self.screen_dimensions.cols + 1;
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
                        self.cursor_position.y =
                            self.text_offset.y + self.screen_dimensions.rows - 1;
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
    let orig_termios = termios::tcgetattr(stdin_raw_fd).expect("Error in tcgetattr");

    let mut termios = orig_termios.clone();
    termios.input_flags &= !(InputFlags::BRKINT
        | InputFlags::ICRNL
        | InputFlags::INPCK
        | InputFlags::ISTRIP
        | InputFlags::IXON);
    termios.output_flags &= !(OutputFlags::OPOST);
    termios.control_flags |= ControlFlags::CS8;
    termios.local_flags &=
        !(LocalFlags::ECHO | LocalFlags::ICANON | LocalFlags::IEXTEN | LocalFlags::ISIG);
    // Rust always blocks when reading from stdin.
    // termios.c_cc[VMIN] = 0;
    // termios.c_cc[VTIME] = 1;
    termios::tcsetattr(stdin_raw_fd, SetArg::TCSAFLUSH, &termios).expect("Error in tcsetattr");

    orig_termios
}

fn disable_raw_mode(orig_termios: &mut Termios) {
    let stdin_raw_fd = io::stdin().as_raw_fd();
    termios::tcsetattr(stdin_raw_fd, SetArg::TCSAFLUSH, orig_termios).expect("Error in tcsetattr");
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

    editor.set_status_message("HELP: Ctrl-S = Save | Ctrl-F = Find | Ctrl-Q = Quit");

    editor.render_loop();
}
