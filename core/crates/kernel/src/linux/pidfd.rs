//! `pidfd_open` and `pidfd_send_signal` helpers for callers that didn't
//! receive a pidfd via `clone3(CLONE_PIDFD)`.
use crate::error::{SandboxError, SandboxResult};
use std::os::fd::{FromRawFd, OwnedFd};

pub fn open(pid: i32) -> SandboxResult<OwnedFd> {
    // SAFETY: pidfd_open is a stable Linux syscall.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        return Err(SandboxError::Internal(format!(
            "pidfd_open: {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: fd is freshly owned by us.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

pub fn send_signal(pidfd: &OwnedFd, signo: i32) -> SandboxResult<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: pidfd_send_signal is a stable Linux syscall.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            signo,
            std::ptr::null::<libc::siginfo_t>(),
            0u32,
        )
    };
    if rc != 0 {
        return Err(SandboxError::Internal(format!(
            "pidfd_send_signal: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}
