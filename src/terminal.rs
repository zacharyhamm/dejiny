use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
use nix::sys::termios::{self, SetArg};
use std::io::Write;
use std::sync::atomic::{AtomicI32, Ordering};

static CLEANUP_FD: AtomicI32 = AtomicI32::new(-1);
// Written once before signal handler is installed, read only from signal handler.
// Using a raw mutable pointer to avoid Rust 2024 static-mut-ref restrictions.
static mut SAVED_TERMIOS_STORAGE: libc::termios = unsafe { std::mem::zeroed() };

fn install_cleanup_handler() {
    extern "C" fn handler(sig: libc::c_int) {
        let fd = CLEANUP_FD.load(Ordering::SeqCst);
        if fd >= 0 {
            unsafe {
                let ptr = std::ptr::addr_of!(SAVED_TERMIOS_STORAGE);
                libc::tcsetattr(fd, libc::TCSANOW, ptr);
            }
        }
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
    }

    let action = SigAction::new(
        SigHandler::Handler(handler),
        SaFlags::empty(),
        SigSet::empty(),
    );
    unsafe {
        let _ = sigaction(Signal::SIGTERM, &action);
        let _ = sigaction(Signal::SIGHUP, &action);
    }
}

/// RAII guard that enters raw mode on construction and restores termios on drop.
/// Handles panics, `?` returns, and normal exit. Optionally installs a signal
/// handler for SIGTERM/SIGHUP as a last resort (Drop doesn't run on signals).
pub struct RawModeGuard {
    original: nix::sys::termios::Termios,
    signal_handler_armed: bool,
}

impl RawModeGuard {
    /// Enter raw mode. If `install_signal_handler` is true, also installs a
    /// SIGTERM/SIGHUP handler that restores termios directly via libc.
    pub fn enter(install_signal_handler: bool) -> anyhow::Result<Self> {
        let original = termios::tcgetattr(std::io::stdin())?;
        let mut raw = original.clone();
        termios::cfmakeraw(&mut raw);

        if install_signal_handler {
            // Store termios in static for signal handler — use libc directly to avoid
            // assuming nix::Termios has the same layout as libc::termios (it doesn't).
            unsafe {
                let ptr = std::ptr::addr_of_mut!(SAVED_TERMIOS_STORAGE);
                if libc::tcgetattr(libc::STDIN_FILENO, ptr) != 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
            }
            CLEANUP_FD.store(libc::STDIN_FILENO, Ordering::SeqCst);
            install_cleanup_handler();
        }

        termios::tcsetattr(std::io::stdin(), SetArg::TCSANOW, &raw)?;

        Ok(Self {
            original,
            signal_handler_armed: install_signal_handler,
        })
    }

    /// Enter raw mode only if stdin is a TTY. Returns `None` if not a TTY.
    pub fn enter_if_tty(install_signal_handler: bool) -> anyhow::Result<Option<Self>> {
        if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
            return Ok(None);
        }
        Ok(Some(Self::enter(install_signal_handler)?))
    }
}

impl RawModeGuard {
    /// Temporarily restore the original (cooked) terminal settings.
    /// Used when suspending dejiny so the user's shell can take over.
    pub fn suspend(&self) {
        let _ = termios::tcsetattr(std::io::stdin(), SetArg::TCSANOW, &self.original);
    }

    /// Re-enter raw mode after a suspend/resume cycle.
    pub fn resume(&self) {
        let mut raw = self.original.clone();
        termios::cfmakeraw(&mut raw);
        let _ = termios::tcsetattr(std::io::stdin(), SetArg::TCSANOW, &raw);
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = termios::tcsetattr(std::io::stdin(), SetArg::TCSANOW, &self.original);
        if self.signal_handler_armed {
            CLEANUP_FD.store(-1, Ordering::SeqCst);
        }
    }
}

/// Reset terminal escape state: SGR attributes, cursor visibility, alternate screen,
/// mouse tracking, bracketed paste, kitty keyboard protocol.
pub fn reset_escape_state(stdout: &mut impl Write) {
    let _ = stdout.write_all(
        b"\x1b[0m\x1b[?25h\x1b[?1049l\x1b[?1000l\x1b[?2004l\x1b[<u\x1b[<u\x1b[<u\x1b[>4;0m",
    );
    let _ = stdout.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_escape_state_output() {
        let mut buf = Vec::new();
        reset_escape_state(&mut buf);
        // Should contain SGR reset
        assert!(buf.windows(4).any(|w| w == b"\x1b[0m"));
        // Should contain cursor show
        assert!(buf.windows(6).any(|w| w == b"\x1b[?25h"));
        // Should contain alternate screen exit
        assert!(buf.windows(8).any(|w| w == b"\x1b[?1049l"));
        // Should not be empty
        assert!(!buf.is_empty());
    }
}
