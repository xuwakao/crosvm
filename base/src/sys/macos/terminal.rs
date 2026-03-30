// macOS terminal mode support — uses POSIX termios (same as Linux).

use std::io::stdin;
use std::io::Stdin;
use std::mem::MaybeUninit;
use std::os::unix::io::AsRawFd;
use std::os::unix::io::RawFd;

use libc::c_int;
use libc::ECHO;
use libc::ICANON;
use libc::ISIG;
use libc::O_NONBLOCK;
use libc::TCSANOW;

use crate::add_fd_flags;
use crate::clear_fd_flags;
use crate::errno::errno_result;
use crate::Result;

fn modify_mode<F: FnOnce(&mut libc::termios)>(fd: RawFd, f: F) -> Result<()> {
    // SAFETY: termios struct initialized by tcgetattr.
    unsafe {
        let mut termios = MaybeUninit::zeroed().assume_init();
        let ret = libc::tcgetattr(fd, &mut termios);
        if ret < 0 {
            return errno_result();
        }
        f(&mut termios);
        let ret = libc::tcsetattr(fd, TCSANOW, &termios);
        if ret < 0 {
            return errno_result();
        }
        Ok(())
    }
}

/// # Safety
/// Implementor must return a genuine terminal file descriptor.
pub unsafe trait Terminal {
    fn tty_fd(&self) -> RawFd;

    fn set_canon_mode(&self) -> Result<()> {
        modify_mode(self.tty_fd(), |t| t.c_lflag |= ICANON | ECHO | ISIG)
    }

    fn set_raw_mode(&self) -> Result<()> {
        modify_mode(self.tty_fd(), |t| t.c_lflag &= !(ICANON | ECHO | ISIG))
    }

    fn set_non_block(&self, non_block: bool) -> Result<()> {
        if non_block {
            add_fd_flags(self.tty_fd(), O_NONBLOCK)
        } else {
            clear_fd_flags(self.tty_fd(), O_NONBLOCK)
        }
    }
}

// SAFETY: Stdin's fd is the process stdin which is a valid terminal fd.
unsafe impl Terminal for Stdin {
    fn tty_fd(&self) -> RawFd {
        stdin().as_raw_fd()
    }
}
