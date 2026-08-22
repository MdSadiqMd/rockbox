//! User-namespace ID mapping (FIX SEC-02).
//!
//! The maps are written by the **parent** (the engine) into
//! `/proc/<child>/...` immediately after `clone3`, then the child is released
//! through a sync pipe. This is the kernel-sanctioned creator-side pattern:
//! the child cannot write its own maps — inside its fresh PID namespace,
//! `/proc/self/{setgroups,uid_map,gid_map}` writes fail with EPERM regardless
//! of the child's capabilities (verified empirically; the pidns makes the
//! proc-file's user namespace mismatch the task's).
//!
//! The child's first act is a blocking read on the sync pipe; everything it
//! does after that (mounts, chown, creds reset) requires the maps to exist.
use crate::error::SandboxResult;

/// Outside-uid the engine maps the in-namespace root to.
pub const NOBODY_UID: u32 = 65534;
pub const NOBODY_GID: u32 = 65534;

/// Parent-side map writes, targeting the freshly-cloned child. Runs on the
/// engine thread right after `clone3`, before the sync-pipe release.
pub fn parent_write_id_maps(child_pid: i32) -> SandboxResult<()> {
    let base = format!("/proc/{child_pid}");
    // setgroups must be denied BEFORE writing gid_map (kernel rule for
    // unprivileged user-NS).
    std::fs::write(format!("{base}/setgroups"), b"deny")?;
    std::fs::write(format!("{base}/uid_map"), b"0 65534 1")?;
    std::fs::write(format!("{base}/gid_map"), b"0 65534 1")?;
    Ok(())
}

/// Child-side wait for the parent's map release. Called as the child's very
/// first act after `clone3`. Raw syscalls only — the child is a fork of a
/// multithreaded process and must not allocate or take user-space locks.
/// Returns an error (via `_exit(101)` in the caller) if the parent died
/// without releasing (read returns 0 = EOF).
pub fn child_wait_for_maps(sync_r: libc::c_int) -> bool {
    // SAFETY: fcntl/read/close are stable syscalls; the pipe fd is valid.
    unsafe {
        // The pipe is created O_NONBLOCK; the child wants a blocking wait.
        libc::fcntl(sync_r, libc::F_SETFL, 0);
        let mut b = 0u8;
        let n = libc::read(sync_r, &mut b as *mut u8 as *mut libc::c_void, 1);
        libc::close(sync_r);
        n == 1
    }
}
