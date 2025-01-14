use crate::common::wcs2zstring;
use crate::flog::FLOG;
use crate::signal::signal_check_cancel;
#[cfg(test)]
use crate::tests::prelude::*;
use crate::wchar::prelude::*;
use crate::wutil::perror;
use errno::{errno, set_errno};
use libc::{c_int, EINTR, FD_CLOEXEC, F_GETFD, F_GETFL, F_SETFD, F_SETFL, O_NONBLOCK};
use nix::fcntl::FcntlArg;
use nix::{fcntl::OFlag, unistd};
use std::ffi::CStr;
use std::io::{self, Read, Write};
use std::os::unix::prelude::*;

pub const PIPE_ERROR: &str = "An error occurred while setting up pipe";

/// The first "high fd", which is considered outside the range of valid user-specified redirections
/// (like >&5).
pub const FIRST_HIGH_FD: RawFd = 10;

/// A sentinel value indicating no timeout.
pub const NO_TIMEOUT: u64 = u64::MAX;

/// A helper type for managing and automatically closing a file descriptor
///
/// This was implemented in rust as a port of the existing C++ code but it didn't take its place
/// (yet) and there's still the original cpp implementation in `src/fds.h`, so its name is
/// disambiguated because some code uses a mix of both for interop purposes.
pub struct AutoCloseFd {
    fd_: RawFd,
}

impl Read for AutoCloseFd {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        nix::unistd::read(self.as_raw_fd(), buf).map_err(std::io::Error::from)
    }
}

impl Write for AutoCloseFd {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        nix::unistd::write(self.as_raw_fd(), buf).map_err(std::io::Error::from)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // We don't buffer anything so this is a no-op.
        Ok(())
    }
}

impl AutoCloseFd {
    // Closes the fd if not already closed.
    pub fn close(&mut self) {
        if self.fd_ != -1 {
            _ = unistd::close(self.fd_);
            self.fd_ = -1;
        }
    }

    // Returns the fd.
    pub fn fd(&self) -> RawFd {
        self.fd_
    }

    // Returns the fd, transferring ownership to the caller.
    pub fn acquire(&mut self) -> RawFd {
        let temp = self.fd_;
        self.fd_ = -1;
        temp
    }

    // Resets to a new fd, taking ownership.
    pub fn reset(&mut self, fd: RawFd) {
        if fd == self.fd_ {
            return;
        }
        self.close();
        self.fd_ = fd;
    }

    // Returns if this has a valid fd.
    pub fn is_valid(&self) -> bool {
        self.fd_ >= 0
    }

    // Create a new AutoCloseFd instance taking ownership of the passed fd
    pub fn new(fd: RawFd) -> Self {
        AutoCloseFd { fd_: fd }
    }

    // Create a new AutoCloseFd without an open fd
    pub fn empty() -> Self {
        AutoCloseFd { fd_: -1 }
    }
}

impl FromRawFd for AutoCloseFd {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        AutoCloseFd { fd_: fd }
    }
}

impl AsRawFd for AutoCloseFd {
    fn as_raw_fd(&self) -> RawFd {
        self.fd()
    }
}

impl AsFd for AutoCloseFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        unsafe { BorrowedFd::borrow_raw(self.fd()) }
    }
}

impl Default for AutoCloseFd {
    fn default() -> AutoCloseFd {
        AutoCloseFd { fd_: -1 }
    }
}

impl Drop for AutoCloseFd {
    fn drop(&mut self) {
        self.close()
    }
}

/// Helper type returned from make_autoclose_pipes.
pub struct AutoClosePipes {
    /// Read end of the pipe.
    pub read: OwnedFd,

    /// Write end of the pipe.
    pub write: OwnedFd,
}

/// Construct a pair of connected pipes, set to close-on-exec.
/// \return None on fd exhaustion.
pub fn make_autoclose_pipes() -> nix::Result<AutoClosePipes> {
    #[allow(unused_mut, unused_assignments)]
    let mut already_cloexec = false;
    #[cfg(HAVE_PIPE2)]
    let pipes = match nix::unistd::pipe2(OFlag::O_CLOEXEC) {
        Ok(pipes) => {
            already_cloexec = true;
            pipes
        }
        Err(err) => {
            FLOG!(warning, PIPE_ERROR);
            perror("pipe2");
            return Err(err);
        }
    };
    #[cfg(not(HAVE_PIPE2))]
    let pipes = match nix::unistd::pipe() {
        Ok(pipes) => pipes,
        Err(err) => {
            FLOG!(warning, PIPE_ERROR);
            perror("pipe");
            return Err(err);
        }
    };

    let readp = unsafe { OwnedFd::from_raw_fd(pipes.0) };
    let writep = unsafe { OwnedFd::from_raw_fd(pipes.1) };

    // Ensure our fds are out of the user range.
    let readp = heightenize_fd(readp, already_cloexec)?;
    let writep = heightenize_fd(writep, already_cloexec)?;

    Ok(AutoClosePipes {
        read: readp,
        write: writep,
    })
}

/// If the given fd is in the "user range", move it to a new fd in the "high range".
/// zsh calls this movefd().
/// \p input_has_cloexec describes whether the input has CLOEXEC already set, so we can avoid
/// setting it again.
/// \return the fd, which always has CLOEXEC set; or an invalid fd on failure, in
/// which case an error will have been printed, and the input fd closed.
fn heightenize_fd(fd: OwnedFd, input_has_cloexec: bool) -> nix::Result<OwnedFd> {
    let raw_fd = fd.as_raw_fd();

    if raw_fd >= FIRST_HIGH_FD {
        if !input_has_cloexec {
            set_cloexec(raw_fd, true);
        }
        return Ok(fd);
    }

    // Here we are asking the kernel to give us a cloexec fd.
    let newfd = match nix::fcntl::fcntl(raw_fd, FcntlArg::F_DUPFD_CLOEXEC(FIRST_HIGH_FD)) {
        Ok(newfd) => newfd,
        Err(err) => {
            perror("fcntl");
            return Err(err);
        }
    };

    Ok(unsafe { OwnedFd::from_raw_fd(newfd) })
}

/// Sets CLO_EXEC on a given fd according to the value of \p should_set.
pub fn set_cloexec(fd: RawFd, should_set: bool /* = true */) -> c_int {
    // Note we don't want to overwrite existing flags like O_NONBLOCK which may be set. So fetch the
    // existing flags and modify them.
    let flags = unsafe { libc::fcntl(fd, F_GETFD, 0) };
    if flags < 0 {
        return -1;
    }
    let mut new_flags = flags;
    if should_set {
        new_flags |= FD_CLOEXEC;
    } else {
        new_flags &= !FD_CLOEXEC;
    }
    if flags == new_flags {
        0
    } else {
        unsafe { libc::fcntl(fd, F_SETFD, new_flags) }
    }
}

/// Wide character version of open() that also sets the close-on-exec flag (atomically when
/// possible).
pub fn wopen_cloexec(
    pathname: &wstr,
    flags: OFlag,
    mode: nix::sys::stat::Mode,
) -> nix::Result<OwnedFd> {
    open_cloexec(wcs2zstring(pathname).as_c_str(), flags, mode)
}

/// Narrow versions of wopen_cloexec.
pub fn open_cloexec(path: &CStr, flags: OFlag, mode: nix::sys::stat::Mode) -> nix::Result<OwnedFd> {
    // Port note: the C++ version of this function had a fallback for platforms where
    // O_CLOEXEC is not supported, using fcntl. In 2023, this is no longer needed.
    let saved_errno = errno();
    errno::set_errno(errno::Errno(0));
    // We retry this in case of signals,
    // if we get EINTR and it's not a SIGINIT, we continue.
    // If it is that's our cancel signal, so we abort.
    loop {
        let ret = nix::fcntl::open(path, flags | OFlag::O_CLOEXEC, mode);
        let ret = ret.map(|raw_fd| unsafe { OwnedFd::from_raw_fd(raw_fd) });

        match ret {
            Ok(fd) => {
                set_errno(saved_errno);
                return Ok(fd);
            }
            Err(err) => {
                if err != nix::Error::EINTR || signal_check_cancel() != 0 {
                    return ret;
                }
            }
        }
    }
}

/// Close a file descriptor \p fd, retrying on EINTR.
pub fn exec_close(fd: RawFd) {
    assert!(fd >= 0, "Invalid fd");
    while unsafe { libc::close(fd) } == -1 {
        if errno::errno().0 != EINTR {
            perror("close");
            break;
        }
    }
}

/// Mark an fd as nonblocking
pub fn make_fd_nonblocking(fd: RawFd) -> Result<(), io::Error> {
    let flags = unsafe { libc::fcntl(fd, F_GETFL, 0) };
    let nonblocking = (flags & O_NONBLOCK) == O_NONBLOCK;
    if !nonblocking {
        match unsafe { libc::fcntl(fd, F_SETFL, flags | O_NONBLOCK) } {
            -1 => return Err(io::Error::last_os_error()),
            _ => return Ok(()),
        };
    }
    Ok(())
}

/// Mark an fd as blocking
pub fn make_fd_blocking(fd: RawFd) -> Result<(), io::Error> {
    let flags = unsafe { libc::fcntl(fd, F_GETFL, 0) };
    let nonblocking = (flags & O_NONBLOCK) == O_NONBLOCK;
    if nonblocking {
        match unsafe { libc::fcntl(fd, F_SETFL, flags & !O_NONBLOCK) } {
            -1 => return Err(io::Error::last_os_error()),
            _ => return Ok(()),
        };
    }
    Ok(())
}

#[test]
#[serial]
fn test_pipes() {
    test_init();
    // Here we just test that each pipe has CLOEXEC set and is in the high range.
    // Note pipe creation may fail due to fd exhaustion; don't fail in that case.
    let mut pipes = vec![];
    for _i in 0..10 {
        if let Ok(pipe) = make_autoclose_pipes() {
            pipes.push(pipe);
        }
    }
    for pipe in pipes {
        for fd in [&pipe.read, &pipe.write] {
            let fd = fd.as_raw_fd();
            assert!(fd >= FIRST_HIGH_FD);
            let flags = unsafe { libc::fcntl(fd, F_GETFD, 0) };
            assert!(flags >= 0);
            assert!(flags & FD_CLOEXEC != 0);
        }
    }
}
