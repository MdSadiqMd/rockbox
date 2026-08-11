//! Capability drop + RLIMIT setup (FIX SEC-24).
//! Called from the child after seccomp install but before final execve
use crate::error::{SandboxError, SandboxResult};
use crate::spec::ResourceLimits;
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

const CAP_LAST_CAP: u32 = 40;

pub fn drop_all_capabilities() -> SandboxResult<()> {
    // Bounding set first — once a cap leaves the bounding set it can never
    // be re-acquired, so this runs before the capset() that empties the
    // inheritable/permitted/effective sets.
    //
    // Runs in the fork child: raw prctl/capset only. Success path is
    // allocation-free (no glibc arena lock before exec).
    for cap in 0..=CAP_LAST_CAP {
        // SAFETY: prctl is a stable syscall.
        let rc = unsafe { libc::prctl(libc::PR_CAPBSET_DROP, cap, 0, 0, 0) };
        if rc != 0 {
            return Err(SandboxError::CapDrop(format!(
                "PR_CAPBSET_DROP {cap}: {}",
                std::io::Error::last_os_error()
            )));
        }
    }
    // SAFETY: PR_CAP_AMBIENT_CLEAR_ALL is a stable prctl.
    let rc = unsafe {
        libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_CLEAR_ALL,
            0,
            0,
            0,
        )
    };
    if rc != 0 {
        return Err(SandboxError::CapDrop(format!(
            "PR_CAP_AMBIENT_CLEAR_ALL: {}",
            std::io::Error::last_os_error()
        )));
    }
    // capset() clears inheritable/permitted/effective in one syscall. The
    // kernel ABI structs aren't all exported by every libc version — define
    // them (identical layout to linux/capability.h v3).
    #[repr(C)]
    struct CapHeader {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CapData {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;
    let header = CapHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let data = [CapData {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    }; 2];
    // SAFETY: capset is a stable syscall; header + data are initialized.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_capset,
            &header as *const CapHeader,
            data.as_ptr() as *const CapData,
        )
    };
    if rc != 0 {
        return Err(SandboxError::CapDrop(format!(
            "capset: {}",
            std::io::Error::last_os_error()
        )));
    }
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
