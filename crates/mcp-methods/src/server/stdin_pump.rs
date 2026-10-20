//! Non-consuming stdin pump (unix).
//!
//! The pump reads a file descriptor with `O_NONBLOCK` set, readiness-driven
//! through [`AsyncFd`], and appends into the transport's
//! [`InputBuffer`](crate::server::stdio::InputBuffer). Unlike a blocking
//! stdin read it never holds bytes outside that buffer, and it stops
//! reading on demand, so a reload can snapshot every byte this process
//! consumed and leave the rest in the OS pipe for the replacement process.

use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;

use tokio::io::unix::AsyncFd;
use tokio::task::JoinHandle;

use crate::server::stdio::{InputBuffer, PumpStep};

/// Borrowed descriptor: the pump never closes it.
struct BorrowedFd(RawFd);

impl AsRawFd for BorrowedFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

fn fcntl_flags(fd: RawFd) -> io::Result<i32> {
    // SAFETY: F_GETFL takes no pointer argument.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(flags)
    }
}

fn set_flags(fd: RawFd, flags: i32) -> io::Result<()> {
    // SAFETY: F_SETFL takes an integer argument.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// A running pump and the descriptor flags to restore.
pub(crate) struct PumpHandle {
    fd: RawFd,
    original_flags: i32,
    task: JoinHandle<()>,
}

impl PumpHandle {
    /// Stop reading and put the descriptor back in its original mode.
    pub(crate) fn stop(self) {
        self.task.abort();
        let _ = set_flags(self.fd, self.original_flags);
    }

    /// Restore the descriptor's original (normally blocking) mode, for an
    /// `exec` that hands it to a new image.
    pub(crate) fn restore_flags(&self) {
        let _ = set_flags(self.fd, self.original_flags);
    }

    /// Re-enter nonblocking mode after a failed `exec`.
    pub(crate) fn reapply_nonblocking(&self) {
        let _ = set_flags(self.fd, self.original_flags | libc::O_NONBLOCK);
    }
}

/// Whether `fd` is a terminal.
pub(crate) fn is_terminal(fd: RawFd) -> bool {
    // SAFETY: isatty takes only the descriptor number.
    unsafe { libc::isatty(fd) == 1 }
}

/// Start pumping `fd` into `input`. Fails (leaving the descriptor as found)
/// when there is no tokio runtime, when `fd` is a terminal, or when the
/// descriptor cannot be registered for readiness (a regular file).
///
/// A terminal is refused up front even though it registers fine: `O_NONBLOCK`
/// lives on the open file description, shared with the user's shell, and is
/// only cleared by [`PumpHandle::stop`], which a signal death never reaches.
pub(crate) fn start(fd: RawFd, input: Arc<InputBuffer>) -> io::Result<PumpHandle> {
    let runtime = tokio::runtime::Handle::try_current()
        .map_err(|e| io::Error::new(io::ErrorKind::Unsupported, e))?;
    if is_terminal(fd) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "stdin is a terminal",
        ));
    }
    let original_flags = fcntl_flags(fd)?;
    set_flags(fd, original_flags | libc::O_NONBLOCK)?;
    let registered = {
        let _enter = runtime.enter();
        // reason: `AsyncFd::register` exists only in the newest tokio; `new` is the
        // form every tokio 1.x accepted, and `BorrowedFd` is never closed.
        #[allow(deprecated)]
        AsyncFd::new(BorrowedFd(fd))
    };
    let afd = match registered {
        Ok(afd) => afd,
        Err(error) => {
            let _ = set_flags(fd, original_flags);
            return Err(io::Error::new(
                error.kind(),
                format!("stdin is not pollable: {error}"),
            ));
        }
    };
    let task = runtime.spawn(run(fd, afd, input));
    Ok(PumpHandle {
        fd,
        original_flags,
        task,
    })
}

async fn run(fd: RawFd, afd: AsyncFd<BorrowedFd>, input: Arc<InputBuffer>) {
    let mut chunk = vec![0u8; 8192];
    loop {
        let mut ready = match afd.readable().await {
            Ok(ready) => ready,
            Err(_) => {
                input.mark_eof_external();
                return;
            }
        };
        loop {
            match input.pump_step(fd, &mut chunk) {
                PumpStep::Data => {}
                PumpStep::WouldBlock => {
                    ready.clear_ready();
                    break;
                }
                PumpStep::Eof => return,
                PumpStep::Frozen => input.wait_unfrozen().await,
            }
        }
    }
}
