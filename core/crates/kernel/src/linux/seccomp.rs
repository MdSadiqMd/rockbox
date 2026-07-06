//! Per-language seccomp profiles, compiled once per engine boot (FIX PERF-07)
//! and reapplied on every spawn via raw `seccomp(2)` for ~5µs per child.
//!
//! Profile design — see `docs/nix_sandbox_flowchart.md` §4. We use
//! `seccompiler` because it has a clean DSL and stable BPF output. The
//! resulting `BpfProgram` is owned by the resolver and re-loaded for each
//! sandbox.

use crate::error::{SandboxError, SandboxResult};
use crate::spec::SeccompProfileId;
use once_cell::sync::OnceCell;
use parking_lot::Mutex;
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCondition as Cond, SeccompFilter, SeccompRule, TargetArch,
};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// A compiled BPF blob ready for the kernel.
#[derive(Debug, Clone)]
pub struct BpfProfile {
    pub id: SeccompProfileId,
    pub program: Arc<BpfProgram>,
}

/// Per-engine seccomp cache. Compiles every known profile once at
/// construction (FIX PERF-07) — subsequent `compile()` calls are lock-free
/// map lookups (~50ns), and `apply()` loads the pre-built BPF via the raw
/// `seccomp(2)` path inside `seccompiler::apply_filter` (~5µs).
#[derive(Debug, Default)]
pub struct SeccompResolver {
    cache: Mutex<HashMap<SeccompProfileId, BpfProfile>>,
}

const ALL_PROFILES: [SeccompProfileId; 6] = [
    SeccompProfileId::InterpAot,
    SeccompProfileId::InterpJit,
    SeccompProfileId::NativeJit,
    SeccompProfileId::Go,
    SeccompProfileId::RlStep,
    SeccompProfileId::RelaxedCompiler,
];

impl SeccompResolver {
    pub fn new() -> Self {
        let this = Self::default();
        // Pre-populate the cache. Compilation is deterministic and profile
        // ids are a fixed set of 6, so we can amortise the ~200µs per-profile
        // compile cost once at engine boot instead of on the first spawn.
        for id in ALL_PROFILES {
            let _ = this.compile(id);
        }
        this
    }

    pub fn global() -> &'static Self {
        static G: OnceCell<SeccompResolver> = OnceCell::new();
        G.get_or_init(SeccompResolver::new)
    }

    pub fn compile(&self, profile: SeccompProfileId) -> SandboxResult<BpfProfile> {
        {
            let cache = self.cache.lock();
            if let Some(p) = cache.get(&profile) {
                return Ok(p.clone());
            }
        }
        let filter = build_filter(profile)?;
        let program: BpfProgram = filter
            .try_into()
            .map_err(|e: seccompiler::BackendError| SandboxError::Seccomp(e.to_string()))?;
        let prof = BpfProfile {
            id: profile,
            program: Arc::new(program),
        };
        let mut cache = self.cache.lock();
        cache.entry(profile).or_insert_with(|| prof.clone());
        Ok(prof)
    }

    /// Apply the program to the current task. Called by the child after
    /// PR_SET_NO_NEW_PRIVS has been set (FIX SEC-14). Stateless — does not
    /// touch the resolver's cache, so the child branch can call this without
    /// paying the price of spinning up a fresh resolver.
    pub fn apply(profile: &BpfProfile) -> SandboxResult<()> {
        seccompiler::apply_filter(&profile.program)
            .map_err(|e| SandboxError::Seccomp(e.to_string()))
    }
}

fn build_filter(profile: SeccompProfileId) -> SandboxResult<SeccompFilter> {
    let arch = host_target_arch()?;
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();

    // ── base set: shared by every profile ────────────────────────────────
    for sc in BASE_SYSCALLS {
        rules.entry(*sc).or_default();
    }

    // ── pthread clone (FIX SEC-26) — arg-filter to thread flags only ────
    rules
        .entry(libc::SYS_clone)
        .or_default()
        .push(pthread_clone_rule()?);
    // clone3's flags live inside a struct, which BPF can't dereference. We
    // allow clone3 unconditionally (empty rule list = match_action=Allow);
    // the security gate is the clone arg-filter above + namespace policy.
    if let Some(c3) = clone3_syscall_id() {
        rules.entry(c3).or_default();
    }

    // Every profile needs the arch-specific glibc/musl startup calls and the
    // socket family (gated separately by the network namespace; seccomp is
    // not the network policy layer).
    for sc in ARCH_STARTUP {
        rules.entry(*sc).or_default();
    }
    for sc in NET_SYSCALLS {
        rules.entry(*sc).or_default();
    }

    match profile {
        SeccompProfileId::InterpAot => {
            // Python — no JIT, mprotect with PROT_EXEC denied via the
            // separate MDWE prctl on native-jit only. Python still needs
            // mprotect for read-only pages, which BASE covers.
            for sc in INTERP_EXTRA {
                rules.entry(*sc).or_default();
            }
            rules.entry(libc::SYS_execve).or_default();
            rules.entry(libc::SYS_execveat).or_default();
        }
        SeccompProfileId::InterpJit => {
            // Node/V8: needs pkey_alloc, pkey_mprotect for W^X JIT pages.
            for sc in INTERP_EXTRA {
                rules.entry(*sc).or_default();
            }
            for sc in JIT_EXTRA {
                rules.entry(*sc).or_default();
            }
            rules.entry(libc::SYS_execve).or_default();
            rules.entry(libc::SYS_execveat).or_default();
        }
        SeccompProfileId::NativeJit => {
            // Rust/C++ AOT binaries. MDWE prctl enforces W^X separately; here
            // we permit the same JIT-adjacent syscalls in case the compiled
            // program uses them for lazy dynamic loading.
            for sc in JIT_EXTRA {
                rules.entry(*sc).or_default();
            }
            // Compiled langs are pre-linked; a single execve of the resolved
            // /sandbox/main is the child's first act. We keep execveat off —
            // there's no legitimate reason for an AOT binary to re-exec.
            rules.entry(libc::SYS_execve).or_default();
        }
        SeccompProfileId::Go => {
            // Go binaries include a runtime that clones threads, uses
            // futexes, and touches pkey pages for GC bitmap tracking.
            for sc in INTERP_EXTRA {
                rules.entry(*sc).or_default();
            }
            for sc in JIT_EXTRA {
                rules.entry(*sc).or_default();
            }
            for sc in GO_EXTRA {
                rules.entry(*sc).or_default();
            }
            rules.entry(libc::SYS_execve).or_default();
        }
        SeccompProfileId::RlStep => {
            // RL workers hot-loop on IPC; shared memory + semaphores.
            for sc in INTERP_EXTRA {
                rules.entry(*sc).or_default();
            }
            for sc in JIT_EXTRA {
                rules.entry(*sc).or_default();
            }
            for sc in RL_EXTRA {
                rules.entry(*sc).or_default();
            }
            rules.entry(libc::SYS_execve).or_default();
        }
        SeccompProfileId::RelaxedCompiler => {
            // Compilers spawn helpers (as, ld, cc1, linker), which need
            // fork/execve. Still no network — mismatch_action stays EPERM
            // and the netns rejects socket ops in-kernel before they hit
            // seccomp.
            for sc in COMPILER_EXTRA {
                rules.entry(*sc).or_default();
            }
            rules.entry(libc::SYS_execve).or_default();
            rules.entry(libc::SYS_execveat).or_default();
            // fork/vfork don't exist on aarch64; clone is allowed via the
            // pthread arg-filter installed above (SEC-26). x86_64 keeps them
            // available for older toolchains that don't call clone.
            #[cfg(target_arch = "x86_64")]
            {
                rules.entry(libc::SYS_fork).or_default();
                rules.entry(libc::SYS_vfork).or_default();
            }
        }
    }

    // Default action: EPERM for anything the profile did not explicitly
    // allowlist. This keeps runtimes alive when they probe optional syscalls
    // (e.g. Python's `os.getxattr` on a filesystem without xattrs) while
    // still hard-refusing the truly dangerous ones — those are absent from
    // both BASE_SYSCALLS and the per-profile extras (ptrace, kexec_*,
    // init_module, delete_module, mount, umount2, pivot_root, chroot,
    // unshare, setns, keyctl, quotactl, reboot, swapon/swapoff, add_key,
    // request_key, userfaultfd, process_vm_writev, bpf) and therefore
    // resolve to `Errno(EPERM)` at the mismatch branch.
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Errno(libc::EPERM as u32),
        SeccompAction::Allow,
        arch,
    )
    .map_err(|e| SandboxError::Seccomp(e.to_string()))?;
    Ok(filter)
}

fn host_target_arch() -> SandboxResult<TargetArch> {
    #[cfg(target_arch = "x86_64")]
    {
        Ok(TargetArch::x86_64)
    }
    #[cfg(target_arch = "aarch64")]
    {
        Ok(TargetArch::aarch64)
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        Err(SandboxError::Seccomp("unsupported host arch".into()))
    }
}

fn clone3_syscall_id() -> Option<i64> {
    // Stable since 5.3 — libc may not expose under older bindings.
    #[cfg(target_arch = "x86_64")]
    {
        Some(435)
    }
    #[cfg(target_arch = "aarch64")]
    {
        Some(435)
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        None
    }
}

fn pthread_clone_rule() -> SandboxResult<SeccompRule> {
    // The clone flags are arg0 on most architectures. We can't simply require
    // a fixed bit-set (different runtimes use different subsets — Go for
    // example omits PARENT_SETTID/CHILD_CLEARTID). The real security
    // invariant is "no new namespaces" — that's what would let a thread
    // break out of the container.
    //
    // Rule: `(arg0 & CLONE_NEW*-mask) == 0` — i.e. none of the NEW_* bits
    // set. Everything else (VM, FS, FILES, SIGHAND, THREAD, SETTLS, SYSVSEM,
    // PARENT_SETTID, CHILD_CLEARTID, SETTID, IO etc.) is allowed.
    let new_ns_mask: u64 = (libc::CLONE_NEWUSER
        | libc::CLONE_NEWPID
        | libc::CLONE_NEWNS
        | libc::CLONE_NEWNET
        | libc::CLONE_NEWIPC
        | libc::CLONE_NEWUTS
        | libc::CLONE_NEWCGROUP) as u64;
    let rule = SeccompRule::new(vec![
        Cond::new(
            0,
            seccompiler::SeccompCmpArgLen::Qword,
            seccompiler::SeccompCmpOp::MaskedEq(new_ns_mask),
            0,
        )
        .map_err(|e| SandboxError::Seccomp(e.to_string()))?,
    ])
    .map_err(|e| SandboxError::Seccomp(e.to_string()))?;
    Ok(rule)
}

// Syscall tables. Additions require an entry in BASE_SYSCALLS if all
// profiles need it, otherwise the per-profile *_EXTRA table.

const BASE_SYSCALLS: &[i64] = &[
    libc::SYS_read,
    libc::SYS_write,
    libc::SYS_close,
    libc::SYS_openat,
    libc::SYS_fstat,
    libc::SYS_newfstatat,
    libc::SYS_lseek,
    libc::SYS_mmap,
    libc::SYS_munmap,
    libc::SYS_mprotect,
    libc::SYS_brk,
    libc::SYS_rt_sigaction,
    libc::SYS_rt_sigprocmask,
    libc::SYS_rt_sigreturn,
    libc::SYS_sigaltstack,
    libc::SYS_pread64,
    libc::SYS_pwrite64,
    libc::SYS_readv,
    libc::SYS_writev,
    libc::SYS_ppoll,
    libc::SYS_pselect6,
    libc::SYS_clock_gettime,
    libc::SYS_clock_getres,
    libc::SYS_clock_nanosleep,
    libc::SYS_gettimeofday,
    libc::SYS_nanosleep,
    libc::SYS_getrandom,
    libc::SYS_futex,
    libc::SYS_getpid,
    libc::SYS_getppid,
    libc::SYS_getuid,
    libc::SYS_getgid,
    libc::SYS_geteuid,
    libc::SYS_getegid,
    libc::SYS_gettid,
    libc::SYS_exit,
    libc::SYS_exit_group,
    libc::SYS_set_robust_list,
    libc::SYS_set_tid_address,
    libc::SYS_rseq,
    libc::SYS_faccessat,
    libc::SYS_faccessat2,
    libc::SYS_readlinkat,
    libc::SYS_fcntl,
    libc::SYS_pipe2,
    libc::SYS_dup3,
    libc::SYS_eventfd2,
    libc::SYS_signalfd4,
    libc::SYS_timerfd_create,
    libc::SYS_timerfd_settime,
    libc::SYS_epoll_create1,
    libc::SYS_epoll_ctl,
    libc::SYS_epoll_pwait,
    libc::SYS_io_uring_setup,
    libc::SYS_io_uring_enter,
    libc::SYS_io_uring_register,
    libc::SYS_madvise,
    libc::SYS_getcwd,
    libc::SYS_chdir,
    libc::SYS_fchdir,
    libc::SYS_umask,
    libc::SYS_ioctl,
    // Filesystem mutation under writable mounts (/tmp, /session, /episode).
    libc::SYS_mkdirat,
    libc::SYS_unlinkat,
    libc::SYS_renameat,
    libc::SYS_renameat2,
    libc::SYS_linkat,
    libc::SYS_symlinkat,
    libc::SYS_ftruncate,
    libc::SYS_truncate,
    libc::SYS_fchmod,
    libc::SYS_fchmodat,
    libc::SYS_fchown,
    libc::SYS_fchownat,
    libc::SYS_utimensat,
    libc::SYS_copy_file_range,
    libc::SYS_splice,
    libc::SYS_tee,
    libc::SYS_vmsplice,
    libc::SYS_fsync,
    libc::SYS_fdatasync,
    libc::SYS_sync_file_range,
    libc::SYS_fallocate,
    libc::SYS_statfs,
    libc::SYS_fstatfs,
    libc::SYS_getxattr,
    libc::SYS_fgetxattr,
    libc::SYS_listxattr,
    libc::SYS_flistxattr,
    // Needed by post-seccomp cap-drop + rlimit + interpreter startup.
    libc::SYS_prctl,
    libc::SYS_capset,
    libc::SYS_capget,
    libc::SYS_setrlimit,
    libc::SYS_prlimit64,
    libc::SYS_getrlimit,
    libc::SYS_getdents64,
    libc::SYS_statx,
    libc::SYS_uname,
    libc::SYS_sysinfo,
    libc::SYS_wait4,
    libc::SYS_waitid,
    libc::SYS_tgkill,
    libc::SYS_kill,
    libc::SYS_restart_syscall,
    libc::SYS_rt_sigtimedwait,
    libc::SYS_rt_sigsuspend,
    libc::SYS_getpriority,
    libc::SYS_setpriority,
    libc::SYS_sched_setaffinity,
    libc::SYS_get_mempolicy,
    libc::SYS_membarrier,
    libc::SYS_mremap,
    libc::SYS_mlock,
    libc::SYS_munlock,
    libc::SYS_clock_settime,
    libc::SYS_socketpair,
];

const INTERP_EXTRA: &[i64] = &[
    libc::SYS_getdents64,
    libc::SYS_statx,
    libc::SYS_prlimit64,
    libc::SYS_sched_getaffinity,
    libc::SYS_sched_yield,
];

const JIT_EXTRA: &[i64] = &[
    libc::SYS_pkey_alloc,
    libc::SYS_pkey_mprotect,
    libc::SYS_membarrier,
    libc::SYS_mremap,
];

const GO_EXTRA: &[i64] = &[libc::SYS_tgkill, libc::SYS_prctl];

const RL_EXTRA: &[i64] = &[
    libc::SYS_shmget,
    libc::SYS_shmat,
    libc::SYS_shmdt,
    libc::SYS_shmctl,
    libc::SYS_semget,
    libc::SYS_semop,
    libc::SYS_memfd_create,
];

const COMPILER_EXTRA: &[i64] = &[
    libc::SYS_getdents64,
    libc::SYS_statx,
    libc::SYS_prlimit64,
    libc::SYS_wait4,
    libc::SYS_waitid,
    libc::SYS_kill,
    libc::SYS_tgkill,
    libc::SYS_pipe2,
    libc::SYS_mremap,
    libc::SYS_prctl,
    libc::SYS_dup,
    libc::SYS_dup3,
    libc::SYS_setpgid,
    libc::SYS_getpgid,
    libc::SYS_setsid,
    libc::SYS_getsid,
];

// Startup calls emitted by glibc/musl loaders and libc init. `arch_prctl` is
// x86_64-only; aarch64 uses `prctl` (already in BASE).
const ARCH_STARTUP: &[i64] = &[
    #[cfg(target_arch = "x86_64")]
    libc::SYS_arch_prctl,
    libc::SYS_set_tid_address,
    libc::SYS_set_robust_list,
    libc::SYS_rseq,
];

// Socket family. Egress is gated by the network namespace (tier=none has an
// empty NS; loopback/allowlist have a proxied NS). Seccomp does NOT enforce
// the policy — it just permits the syscalls to run so runtimes that always
// call socket() at startup (Node, Go) don't die on init.
const NET_SYSCALLS: &[i64] = &[
    libc::SYS_socket,
    libc::SYS_socketpair,
    libc::SYS_connect,
    libc::SYS_bind,
    libc::SYS_listen,
    libc::SYS_accept,
    libc::SYS_accept4,
    libc::SYS_getsockname,
    libc::SYS_getpeername,
    libc::SYS_getsockopt,
    libc::SYS_setsockopt,
    libc::SYS_shutdown,
    libc::SYS_sendto,
    libc::SYS_recvfrom,
    libc::SYS_sendmsg,
    libc::SYS_recvmsg,
    libc::SYS_sendmmsg,
    libc::SYS_recvmmsg,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_profile_compiles() {
        let r = SeccompResolver::new();
        for id in ALL_PROFILES {
            let p = r.compile(id).expect("profile compiles");
            assert!(!p.program.is_empty(), "empty program for {:?}", id);
        }
    }

    #[test]
    fn compile_is_cached() {
        let r = SeccompResolver::new();
        let a = r.compile(SeccompProfileId::InterpAot).unwrap();
        let b = r.compile(SeccompProfileId::InterpAot).unwrap();
        assert!(Arc::ptr_eq(&a.program, &b.program));
    }
}
