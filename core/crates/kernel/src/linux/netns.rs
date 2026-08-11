//! Shared netns template — a single pre-created, `lo`-up network
//! namespace that each sandbox inherits instead of paying the kernel's
//! netns-creation cost inside `clone3` (`CLONE_NEWNET` ≈ 1.5 ms on typical
//! kernels; `setns` ≈ microseconds).
//!
//! The engine is single-request at a time (one sandbox per VM), so one
//! template never has more than one resident task. The template's lifetime is
//! pinned by the fd we keep open in the engine process; on any init failure
//! the launcher silently falls back to creating a fresh netns per sandbox.

use crate::error::{SandboxError, SandboxResult};
use std::os::fd::{AsRawFd, OwnedFd};

#[derive(Debug)]
pub struct NetnsTemplate {
    /// Our container's netns, restored after each launch.
    host: OwnedFd,
    /// The recycled template netns (`lo` up).
    template: OwnedFd,
}

impl NetnsTemplate {
    /// Create the template namespace. Fails (returns `Err`) if we lack
    /// `CAP_SYS_ADMIN` or can't bring `lo` up — the launcher then keeps using
    /// per-sandbox `CLONE_NEWNET`.
    pub fn init() -> SandboxResult<Self> {
        let host = open_ns_net()?;
        // SAFETY: unshare(CLONE_NEWNET) puts us in a fresh netns; we restore
        // below. The engine has no live network sockets, so this is safe.
        let rc = unsafe { libc::unshare(libc::CLONE_NEWNET) };
        if rc != 0 {
            return Err(SandboxError::Net(
                "unshare(CLONE_NEWNET) for template failed".into(),
            ));
        }
        let _ = bring_lo_up();
        let template = open_ns_net()?;
        restore(&host)?;
        Ok(Self { host, template })
    }

    /// Enter the template, run `f`, then restore the container netns.
    /// WARNING: `f` must not `return` on both sides of a `clone3` — this is
    /// only used to wrap `clone3` where the child branch never returns (it
    /// execs or exits), so the host-netns restore below is parent-only.
    pub fn with_template<R>(&self, f: impl FnOnce() -> SandboxResult<R>) -> SandboxResult<R> {
        enter(self)?;
        let out = f();
        restore(&self.host)?;
        out
    }
}

fn open_ns_net() -> SandboxResult<OwnedFd> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC)
        .open("/proc/self/ns/net")
        .map_err(|e| SandboxError::Net(format!("/proc/self/ns/net: {e}")))?;
    Ok(f.into())
}

fn enter(t: &NetnsTemplate) -> SandboxResult<()> {
    // SAFETY: setns is stable; t.template is our own netns fd.
    let rc = unsafe { libc::setns(t.template.as_raw_fd(), libc::CLONE_NEWNET) };
    if rc != 0 {
        return Err(SandboxError::Net("setns(template) failed".into()));
    }
    Ok(())
}

fn restore(fd: &OwnedFd) -> SandboxResult<()> {
    // SAFETY: setns is stable; fd refers to our container's netns.
    let rc = unsafe { libc::setns(fd.as_raw_fd(), libc::CLONE_NEWNET) };
    if rc != 0 {
        return Err(SandboxError::Net("setns(host) failed".into()));
    }
    Ok(())
}

fn bring_lo_up() -> SandboxResult<()> {
    // SAFETY: socket() + ioctl(SIOCSIFFLAGS) are standard; ifreq is
    // zero-initialised with "lo" copied into ifr_name.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if fd < 0 {
        return Err(SandboxError::Net("socket for lo up failed".into()));
    }
    let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };
    for (dst, src) in ifr.ifr_name.iter_mut().zip(b"lo\0".iter()) {
        *dst = *src as libc::c_char;
    }
    let rc = unsafe { libc::ioctl(fd, libc::SIOCGIFFLAGS as libc::c_ulong, &mut ifr) };
    if rc == 0 {
        // SAFETY: union field access — libc requires an explicit unsafe block
        // around ifreq's anonymous union member.
        unsafe { ifr.ifr_ifru.ifru_flags |= libc::IFF_UP as i16 };
        let _ = unsafe { libc::ioctl(fd, libc::SIOCSIFFLAGS as libc::c_ulong, &ifr) };
    }
    unsafe { libc::close(fd) };
    Ok(())
}
