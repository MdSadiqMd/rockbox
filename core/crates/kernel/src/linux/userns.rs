//! User-namespace ID mapping. Done from the **parent** after `clone3` so the
//! child can be paused on a sync pipe until the map is written (FIX SEC-02).
use crate::error::{SandboxError, SandboxResult};
use std::os::fd::{AsRawFd, OwnedFd};

/// Outside-uid the engine maps the in-namespace root to.
pub const NOBODY_UID: u32 = 65534;
pub const NOBODY_GID: u32 = 65534;

pub fn write_id_maps(child_pid: i32) -> SandboxResult<()> {
    write_file(
        &format!("/proc/{child_pid}/uid_map"),
        &format!("0 {NOBODY_UID} 1"),
    )?;
    // setgroups must be denied BEFORE writing gid_map (kernel rule for
    // unprivileged user-NS).
    write_file(&format!("/proc/{child_pid}/setgroups"), "deny")?;
    write_file(
        &format!("/proc/{child_pid}/gid_map"),
        &format!("0 {NOBODY_GID} 1"),
    )?;
    Ok(())
}

/// Pipe pair the parent uses to release the child once uid_map has been
/// written. The child blocks on a 1-byte read; the parent writes "x" to release.
#[derive(Debug)]
pub struct SyncPipe {
    pub read_end: OwnedFd,
    pub write_end: OwnedFd,
}

pub fn make_sync_pipe() -> SandboxResult<SyncPipe> {
    use nix::fcntl::OFlag;
    use nix::unistd::pipe2;
    let (r, w) =
        pipe2(OFlag::O_CLOEXEC).map_err(|e| SandboxError::UserNs(format!("pipe2: {e}")))?;
    Ok(SyncPipe {
        read_end: r,
        write_end: w,
    })
}

/// Wake the child once uid_map has been written. The parent should call this
/// after [`write_id_maps`] returns; the child wakes from the read in
/// [`child_wait_for_release`].
pub fn release_child(write_end: &OwnedFd) -> SandboxResult<()> {
    // nix 0.29 takes RawFd, not OwnedFd; use as_raw_fd() to keep ownership.
    // SAFETY: write to a pipe fd is always safe; the OwnedFd outlives the call.
    let n = unsafe { libc::write(write_end.as_raw_fd(), b"x".as_ptr() as *const _, 1) };
    if n != 1 {
        return Err(SandboxError::UserNs(format!(
            "release write: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

pub fn child_wait_for_release(read_end: &OwnedFd) -> SandboxResult<()> {
    let mut buf = [0u8; 1];
    // SAFETY: read from a pipe fd is always safe; the OwnedFd outlives the call.
    let n = unsafe { libc::read(read_end.as_raw_fd(), buf.as_mut_ptr() as *mut _, 1) };
    if n < 0 {
        return Err(SandboxError::UserNs(format!(
            "child sync read: {}",
            std::io::Error::last_os_error()
        )));
    }
    if n == 0 {
        // EOF means all writers closed before we got the release byte —
        // proceeding without `uid_map` would create files owned by the
        // overflow_uid and mkdir() would fail with EOVERFLOW down the line.
        return Err(SandboxError::UserNs(
            "child sync read: parent closed pipe before signalling release (EOF)".into(),
        ));
    }
    Ok(())
}

fn write_file(path: &str, contents: &str) -> SandboxResult<()> {
    std::fs::write(path, contents).map_err(|e| SandboxError::UserNs(format!("{path}: {e}")))
}
