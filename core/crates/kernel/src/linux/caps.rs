//! Capability drop + RLIMIT setup (FIX SEC-24).
//! Called from the child after seccomp install but before final execve
use crate::error::{SandboxError, SandboxResult};
use crate::spec::ResourceLimits;
use caps::{CapSet, Capability};
use nix::sys::resource::{Resource, setrlimit};

pub fn set_no_new_privs() -> SandboxResult<()> {
    // SAFETY: prctl is a stable syscall.
    let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if rc != 0 {
        return Err(SandboxError::CapDrop(format!(
            "prctl PR_SET_NO_NEW_PRIVS: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

pub fn enable_mdwe() -> SandboxResult<()> {
    // PR_SET_MDWE = 65; flag PR_MDWE_REFUSE_EXEC_GAIN = 1.
    // Kernel >= 6.3.
    const PR_SET_MDWE: i32 = 65;
    const PR_MDWE_REFUSE_EXEC_GAIN: u64 = 1;
    let rc = unsafe { libc::prctl(PR_SET_MDWE, PR_MDWE_REFUSE_EXEC_GAIN, 0, 0, 0) };
    if rc != 0 {
        // Non-fatal — older kernels lack this. Caller logs.
        return Err(SandboxError::CapDrop(format!(
            "prctl PR_SET_MDWE: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

pub fn drop_all_capabilities() -> SandboxResult<()> {
    // Drop bounding, ambient, inheritable, effective, permitted in that order.
    caps::clear(None, CapSet::Bounding).map_err(cap_err)?;
    caps::clear(None, CapSet::Ambient).map_err(cap_err)?;
    caps::clear(None, CapSet::Inheritable).map_err(cap_err)?;
    caps::clear(None, CapSet::Effective).map_err(cap_err)?;
    caps::clear(None, CapSet::Permitted).map_err(cap_err)?;
    debug_assert!(
        caps::read(None, CapSet::Effective)
            .unwrap_or_default()
            .is_empty()
    );
    // Silence unused-import warning when assertions are off.
    let _ = Capability::CAP_SYS_ADMIN;
    Ok(())
}

pub fn apply_rlimits(lim: &ResourceLimits) -> SandboxResult<()> {
    set(Resource::RLIMIT_FSIZE, lim.fsize_bytes, "FSIZE")?;
    set(Resource::RLIMIT_NPROC, u64::from(lim.pids_max), "NPROC")?;
    set(Resource::RLIMIT_NOFILE, u64::from(lim.nofile), "NOFILE")?;
    // RLIMIT_AS deliberately NOT set: V8 (Node, JIT engines, Wasm, Rust+tokio
    // address-space sandboxing) reserves multi-GB of virtual memory regardless
    // of actual heap usage. Capping AS strangles them. Real memory bounds are
    // enforced via cgroup `memory.max` (PERF-10), which is the only thing
    // that maps to RAM-level isolation anyway.
    let _ = lim.address_space_bytes;
    set(Resource::RLIMIT_STACK, lim.stack_bytes, "STACK")?;
    set(Resource::RLIMIT_CORE, 0, "CORE")?;
    Ok(())
}

fn set(res: Resource, val: u64, name: &'static str) -> SandboxResult<()> {
    setrlimit(res, val, val).map_err(|e| SandboxError::RLimit {
        limit: name,
        source: std::io::Error::from_raw_os_error(e as i32),
    })
}

fn cap_err(e: caps::errors::CapsError) -> SandboxError {
    SandboxError::CapDrop(e.to_string())
}
