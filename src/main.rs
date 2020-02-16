use std::io::{self, Read};
use std::os::unix::io::AsRawFd;
use termios::*;

/*** terminal ***/

fn enable_raw_mode() -> Termios {
    let stdin = io::stdin();
    let orig_termios =
        Termios::from_fd(stdin.as_raw_fd()).expect("Error creating termios");

    let mut termios = orig_termios.clone();
    termios.c_iflag &= !(BRKINT | ICRNL | INPCK | ISTRIP | IXON);
    termios.c_oflag &= !(OPOST);
    termios.c_cflag |= CS8;
    termios.c_lflag &= !(ECHO | ICANON | IEXTEN | ISIG);
    // Rust always blocks when reading from stdin.
    // termios.c_cc[VMIN] = 0;
    // termios.c_cc[VTIME] = 1;
    tcsetattr(stdin.as_raw_fd(), TCSAFLUSH, &mut termios)
        .expect("Error in tcsetattr");

    orig_termios
}

fn disable_raw_mode(orig_termios: &mut Termios) {
    let stdin = io::stdin();
    tcsetattr(stdin.as_raw_fd(), TCSAFLUSH, orig_termios)
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

/*** init ***/

fn main() {
    let stdin = io::stdin();
    let orig_termios = enable_raw_mode();
    // Disable raw mode when this struct is dropped.
    let _raw_mode_disabler = RawModeDisabler { orig_termios };

    // This blocks until there is a byte to read.
    for byte in stdin.bytes() {
        let c = byte.expect("Error reading byte") as char;
        if c.is_control() {
            print!("{}\r\n", c as u8)
        } else {
            print!("{} ('{}')\r\n", c as u8, c);
        }
        if c == 'q' {
            break;
        }
    }
}
