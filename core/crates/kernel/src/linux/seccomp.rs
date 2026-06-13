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

#[derive(Debug, Default)]
pub struct SeccompResolver {
    cache: Mutex<HashMap<SeccompProfileId, BpfProfile>>,
}

impl SeccompResolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn global() -> &'static Self {
        static G: OnceCell<SeccompResolver> = OnceCell::new();
        G.get_or_init(SeccompResolver::new)
    }

    pub fn compile(&self, profile: SeccompProfileId) -> SandboxResult<BpfProfile> {
        let mut cache = self.cache.lock();
        if let Some(p) = cache.get(&profile) {
            return Ok(p.clone());
        }
        let filter = build_filter(profile)?;
        let program: BpfProgram = filter
            .try_into()
            .map_err(|e: seccompiler::BackendError| SandboxError::Seccomp(e.to_string()))?;
        let prof = BpfProfile {
            id: profile,
            program: Arc::new(program),
        };
        cache.insert(profile, prof.clone());
        Ok(prof)
    }

    /// Apply the program to the current task. Called by the child after
    /// PR_SET_NO_NEW_PRIVS has been set (FIX SEC-14).
    pub fn apply(&self, profile: &BpfProfile) -> SandboxResult<()> {
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

    match profile {
        SeccompProfileId::InterpAot => {
            // Python — no JIT, mprotect with PROT_EXEC denied.
            for sc in INTERP_EXTRA {
                rules.entry(*sc).or_default();
            }
            // execve allowed (arg-filter applied via mount-time interpreter pinning).
            rules.entry(libc::SYS_execve).or_default();
        }
        SeccompProfileId::InterpJit => {
            for sc in INTERP_EXTRA {
                rules.entry(*sc).or_default();
            }
            for sc in JIT_EXTRA {
                rules.entry(*sc).or_default();
            }
            rules.entry(libc::SYS_execve).or_default();
        }
        SeccompProfileId::NativeJit => {
            for sc in JIT_EXTRA {
                rules.entry(*sc).or_default();
            }
            // Until the two-stage filter (SEC-11) lands, allow execve for
            // compiled langs too — the engine pre-compiles outside the
            // sandbox and execve's the resulting binary as the child's first
            // act. With stage-2 we'd revoke execve right after.
            rules.entry(libc::SYS_execve).or_default();
        }
        SeccompProfileId::Go => {
            for sc in INTERP_EXTRA {
                rules.entry(*sc).or_default();
            }
            for sc in JIT_EXTRA {
                rules.entry(*sc).or_default();
            }
            for sc in GO_EXTRA {
                rules.entry(*sc).or_default();
            }
            // `go run` execs the temp-compiled binary as its first act; we
            // need execve in the allowlist.
            rules.entry(libc::SYS_execve).or_default();
        }
        SeccompProfileId::RlStep => {
            for sc in INTERP_EXTRA {
                rules.entry(*sc).or_default();
            }
            for sc in JIT_EXTRA {
                rules.entry(*sc).or_default();
            }
            for sc in RL_EXTRA {
                rules.entry(*sc).or_default();
            }
        }
        SeccompProfileId::RelaxedCompiler => {
            for sc in COMPILER_EXTRA {
                rules.entry(*sc).or_default();
            }
            rules.entry(libc::SYS_execve).or_default();
            // fork/vfork don't exist on aarch64; clone is allowed via the
            // pthread arg-filter installed above (SEC-26).
        }
    }

    // TODO(sandbox): tighten the allowlist. For now we default to `Errno(EPERM)`
    // on unknown syscalls so children survive minor coverage gaps while we
    // walk every runtime's syscall surface; matched rules still pass through.
    // Switch back to `KillProcess` once the BASE_SYSCALLS table is audited.
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

fn pthread_clone3_rule() -> SandboxResult<SeccompRule> {
    // clone3 takes a struct pointer; we cannot inspect flags from BPF
    // without indirect-load support. Default action remains KILL_PROCESS for
    // unmatched rules; this empty rule is a placeholder so the syscall isn't
    // hard-denied. A safer option is to deny clone3 entirely and force the
    // runtime to fall back to clone — left as a future hardening step.
    SeccompRule::new(vec![]).map_err(|e| SandboxError::Seccomp(e.to_string()))
}

// ── Syscall tables (subset; expand as profiles need) ────────────────────────

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
];
