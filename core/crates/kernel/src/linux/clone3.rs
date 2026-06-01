//! `clone3(2)` wrapper covering the fused-spawn pattern (PERF-06 + SEC-17):
//!
//! ```text
//! clone3(NEWUSER | NEWPID | NEWNS | NEWNET | NEWIPC | NEWUTS
//!      | CLONE_PIDFD | CLONE_INTO_CGROUP | CLONE_CLEAR_SIGHAND)
//! ```
//!
//! One syscall produces the child, a pidfd owned by the parent, and direct
//! cgroup placement — removing the historical post-clone races on uid_map
//! ordering, cgroup write, and pidfd_open.

use crate::error::{SandboxError, SandboxResult};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

/// Subset of `clone_args` we actually populate. Rest are zero (default behaviour).
#[repr(C)]
#[derive(Default)]
struct CloneArgs {
    flags: u64,
    pidfd: u64,
    child_tid: u64,
    parent_tid: u64,
    exit_signal: u64,
    stack: u64,
    stack_size: u64,
    tls: u64,
    set_tid: u64,
    set_tid_size: u64,
    cgroup: u64,
}

// CLONE_INTO_CGROUP not yet in libc 0.2 on all targets; define manually.
const CLONE_INTO_CGROUP: u64 = 0x2000_0000_0000;
const CLONE_PIDFD: u64 = 0x0000_1000;
const CLONE_CLEAR_SIGHAND: u64 = 0x1_0000_0000;

/// Result of a successful `clone3`.
///
/// The child branch returns `pidfd: None` — only the parent owns the pidfd.
/// Constructing an `OwnedFd` from `-1` is undefined behaviour on modern
/// `std` versions, so the child path doesn't even try.
#[derive(Debug)]
pub struct Cloned {
    pub child_pid: i32,
    pub pidfd: Option<OwnedFd>,
}

#[derive(Debug)]
pub struct CloneRequest {
    pub namespaces: u64,
    /// Cgroup fd to enter atomically (`O_PATH | O_DIRECTORY` on `cgroup.fd`).
    pub cgroup_fd: Option<RawFd>,
    /// SIGCHLD by default; pass 0 to suppress.
    pub exit_signal: u64,
}

impl CloneRequest {
    pub const fn full_isolation() -> u64 {
        // libc constants — keep as u64 for clone3.
        use libc::*;
        (CLONE_NEWUSER | CLONE_NEWPID | CLONE_NEWNS | CLONE_NEWNET | CLONE_NEWIPC | CLONE_NEWUTS)
            as u64
    }
}

pub fn clone3(req: CloneRequest) -> SandboxResult<Cloned> {
    let mut pidfd: i32 = -1;
    let mut args = CloneArgs {
        flags: req.namespaces | CLONE_PIDFD | CLONE_CLEAR_SIGHAND,
        pidfd: &mut pidfd as *mut i32 as u64,
        exit_signal: req.exit_signal,
        ..Default::default()
    };
    if let Some(fd) = req.cgroup_fd {
        args.flags |= CLONE_INTO_CGROUP;
        args.cgroup = fd as u64;
    }

    // SAFETY: clone3 is well-defined; we pass a properly-sized `clone_args`
    // and the pidfd output pointer is valid for the duration of the call.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_clone3,
            &args as *const CloneArgs,
            size_of::<CloneArgs>(),
        )
    };

    if rc < 0 {
        let err = std::io::Error::last_os_error();
        return Err(SandboxError::Clone(format!("clone3 errno={err}")));
    }
    let child_pid = rc as i32;
    if child_pid == 0 {
        // We are the child. The pidfd is owned exclusively by the parent;
        // we don't even try to wrap it (constructing `OwnedFd::from_raw_fd(-1)`
        // is UB on modern std and panics in debug).
        return Ok(Cloned {
            child_pid: 0,
            pidfd: None,
        });
    }

    // Parent path.
    if pidfd < 0 {
        return Err(SandboxError::Clone("clone3 returned no pidfd".into()));
    }
    // SAFETY: pidfd is owned by us (CLONE_PIDFD contract).
    let owned = unsafe { OwnedFd::from_raw_fd(pidfd) };
    Ok(Cloned {
        child_pid,
        pidfd: Some(owned),
    })
}
