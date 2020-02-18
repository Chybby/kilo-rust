use nix::sys::termios::{
    self, ControlFlags, InputFlags, LocalFlags, OutputFlags, SetArg, Termios,
};
use std::cmp;
use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use term_size;

const VERSION: &str = "0.0.1";

/*** macros ***/

/*** terminal ***/

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
    termios::tcsetattr(stdin_raw_fd, SetArg::TCSAFLUSH, &mut termios)
        .expect("Error in tcsetattr");

    orig_termios
}

fn disable_raw_mode(orig_termios: &mut Termios) {
    let stdin_raw_fd = io::stdin().as_raw_fd();
    termios::tcsetattr(stdin_raw_fd, SetArg::TCSAFLUSH, orig_termios)
        .expect("Error in tcsetattr");
}

struct RawModeDisabler {
    orig_termios: Termios,
}

impl Drop for RawModeDisabler {
    fn drop(&mut self) {
        disable_raw_mode(&mut self.orig_termios);
    }
}

// Create a way to read from stdin without blocking.
fn spawn_stdin_channel() -> Receiver<u8> {
    let (tx, rx) = mpsc::channel::<u8>();
    thread::spawn(move || loop {
        let mut buf = [0];
        io::stdin().read(&mut buf).unwrap();
        tx.send(buf[0]).unwrap();
    });
    rx
}

enum Arrow {
    Left,
    Right,
    Up,
    Down,
}

enum Key {
    Char(char),
    Ctrl(char),
    Arrow(Arrow),
    PageUp,
    PageDown,
    Home,
    End,
    Delete,
    Esc,
}

fn editor_read_key(input: &Receiver<u8>) -> Key {
    // TODO: this is a bit of a mess.
    match input.recv() {
        Ok(byte) => {
            // Handling an escape sequence.
            if byte == b'\x1b' {
                // Try to read the rest of the escape sequence.
                match input.try_recv() {
                    Ok(b'[') => {
                        match input.try_recv() {
                            Ok(b'A') => Key::Arrow(Arrow::Up),
                            Ok(b'B') => Key::Arrow(Arrow::Down),
                            Ok(b'C') => Key::Arrow(Arrow::Right),
                            Ok(b'D') => Key::Arrow(Arrow::Left),
                            Ok(b'H') => Key::Home,
                            Ok(b'F') => Key::End,
                            Ok(n @ b'0'..=b'9') => match input.try_recv() {
                                Ok(b'~') => match n {
                                    b'1' | b'7' => Key::Home,
                                    b'4' | b'8' => Key::End,
                                    b'3' => Key::Delete,
                                    b'5' => Key::PageUp,
                                    b'6' => Key::PageDown,
                                    _ => Key::Esc,
                                },
                                // Ignore all three bytes after the esc.
                                Ok(_) | Err(TryRecvError::Empty) => Key::Esc,
                                Err(TryRecvError::Disconnected) => {
                                    panic!("Input channel disconnected")
                                }
                            },
                            Ok(b'O') => match input.try_recv() {
                                Ok(b'H') => Key::Home,
                                Ok(b'F') => Key::End,
                                // Ignore all three bytes after the esc.
                                Ok(_) | Err(TryRecvError::Empty) => Key::Esc,
                                Err(TryRecvError::Disconnected) => {
                                    panic!("Input channel disconnected")
                                }
                            },
                            // Ignore both bytes after the esc.
                            Ok(_) | Err(TryRecvError::Empty) => Key::Esc,
                            Err(TryRecvError::Disconnected) => {
                                panic!("Input channel disconnected")
                            }
                        }
                    }
                    // Ignore the byte after the esc.
                    Ok(_) | Err(TryRecvError::Empty) => Key::Esc,
                    Err(TryRecvError::Disconnected) => {
                        panic!("Input channel disconnected")
                    }
                }
            // Handling any other byte.
            } else {
                let c = byte as char;
                if c.is_control() {
                    Key::Ctrl((c as u8 | 0b01100000) as char)
                } else {
                    Key::Char(c)
                }
            }
        }
        Err(_) => panic!("Error reading from input channel"),
    }
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

/*** output ***/

// TODO: abstract away all drawing to some struct.
fn editor_clear_screen(contents: &mut String) {
    // Clear the whole screen.
    contents.push_str("\x1b[2J");
}

fn editor_clear_row(contents: &mut String) {
    // Clear the current row from the cursor to the end.
    contents.push_str("\x1b[K");
}

fn editor_reset_cursor(contents: &mut String) {
    // Move the cursor to the top left.
    editor_draw_cursor(contents, &Position { x: 0, y: 0 });
}

fn editor_draw_cursor(contents: &mut String, cursor_position: &Position) {
    // Move the displayed cursor to a certain position.
    let s =
        format!("\x1b[{};{}H", cursor_position.y + 1, cursor_position.x + 1);
    contents.push_str(&s);
}

fn editor_hide_cursor(contents: &mut String) {
    // Make the cursor invisible.
    contents.push_str("\x1b[?25l");
}

fn editor_show_cursor(contents: &mut String) {
    // Make the cursor visible.
    contents.push_str("\x1b[?25h");
}

fn editor_draw_rows(editor_state: &EditorState, contents: &mut String) {
    for y in 0..editor_state.screen_dimensions.rows {
        if y == editor_state.screen_dimensions.rows / 3 {
            let welcome_message = format!("Kilo editor -- version {}", VERSION);
            let message_length = cmp::min(
                welcome_message.len(),
                editor_state.screen_dimensions.cols - 1,
            );

            let mut padding =
                (editor_state.screen_dimensions.cols - message_length) / 2;
            if padding > 0 {
                contents.push_str("~");
                padding -= 1;
            }

            for _ in 0..padding {
                contents.push_str(" ");
            }

            contents.push_str(&welcome_message[..message_length]);
        } else {
            contents.push_str("~");
        }
        editor_clear_row(contents);

        // Add a newline to all but the last line.
        if y < editor_state.screen_dimensions.rows - 1 {
            contents.push_str("\r\n");
        }
    }
}

fn editor_reset_screen() {
    let mut contents = String::new();

    editor_clear_screen(&mut contents);
    editor_reset_cursor(&mut contents);

    print!("{}", contents);
    io::stdout().flush().unwrap();
}

fn editor_refresh_screen(editor_state: &EditorState) {
    let mut contents = String::new();

    editor_hide_cursor(&mut contents);
    editor_reset_cursor(&mut contents);

    editor_draw_rows(editor_state, &mut contents);

    editor_draw_cursor(&mut contents, &editor_state.cursor_position);

    editor_show_cursor(&mut contents);

    print!("{}", contents);
    io::stdout().flush().unwrap();
}

/*** input ***/

enum KeypressResult {
    Continue,
    Terminate,
}

fn editor_move_cursor(
    editor_state: &mut EditorState,
    arrow: Arrow,
) -> KeypressResult {
    match arrow {
        Arrow::Up => {
            if editor_state.cursor_position.y > 0 {
                editor_state.cursor_position.y -= 1
            }
        }
        Arrow::Left => {
            if editor_state.cursor_position.x > 0 {
                editor_state.cursor_position.x -= 1
            }
        }
        Arrow::Down => {
            if editor_state.cursor_position.y
                < editor_state.screen_dimensions.rows - 1
            {
                editor_state.cursor_position.y += 1
            }
        }
        Arrow::Right => {
            if editor_state.cursor_position.x
                < editor_state.screen_dimensions.cols - 1
            {
                editor_state.cursor_position.x += 1
            }
        }
    };
    KeypressResult::Continue
}

fn editor_process_keypress(editor_state: &mut EditorState) -> KeypressResult {
    let key = editor_read_key(&editor_state.input);

    match key {
        Key::Ctrl('q') => KeypressResult::Terminate,
        Key::Arrow(arrow) => editor_move_cursor(editor_state, arrow),
        key @ Key::PageUp | key @ Key::PageDown => {
            for _ in 0..editor_state.screen_dimensions.rows {
                editor_move_cursor(
                    editor_state,
                    if let Key::PageUp = key {
                        Arrow::Up
                    } else {
                        Arrow::Down
                    },
                );
            }
            KeypressResult::Continue
        }
        Key::Home => {
            editor_state.cursor_position.x = 0;
            KeypressResult::Continue
        }
        Key::End => {
            editor_state.cursor_position.x =
                editor_state.screen_dimensions.cols - 1;
            KeypressResult::Continue
        }
        _ => KeypressResult::Continue,
    }
}

trait Control {
    fn is_ctrl(self, c: char) -> bool;
}

impl Control for char {
    fn is_ctrl(self, c: char) -> bool {
        return (c as u8) & 0b00011111 == self as u8;
    }
}

/*** init ***/

struct Position {
    x: usize,
    y: usize,
}

struct Dimensions {
    rows: usize,
    cols: usize,
}

struct EditorState {
    screen_dimensions: Dimensions,
    cursor_position: Position,
    input: Receiver<u8>,
}

impl EditorState {
    fn new() -> EditorState {
        let screen_dimensions = get_window_size();

        EditorState {
            screen_dimensions,
            cursor_position: Position { x: 0, y: 0 },
            input: spawn_stdin_channel(),
        }
    }
}

fn main() {
    // Enabling raw mode.
    let orig_termios = enable_raw_mode();
    // Disable raw mode when this struct is dropped.
    let _raw_mode_disabler = RawModeDisabler { orig_termios };

    let mut editor_state = EditorState::new();

    loop {
        editor_refresh_screen(&editor_state);
        if let KeypressResult::Terminate =
            editor_process_keypress(&mut editor_state)
        {
            break;
        }
    }

    editor_reset_screen();
}
