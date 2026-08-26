//! Platform-neutral specifications fed into the sandbox launcher.
//!
//! These structs are the **only** input to the launcher. They are derived
//! from a frozen `Settings` object by `engine::resolver`, which keeps
//! all settings-to-syscall mapping in one place.

use protocol::Language;
use std::path::PathBuf;
use std::time::Duration;

/// fd number the child's protocol pipe is dup2'd to when the spec requests
/// one ([`ChildSpec::protocol_fd`]). Persistent RL workers exchange framed
/// request/response messages over it, leaving stdout for user output.
pub const PROTOCOL_FD: i32 = 3;

/// Everything the launcher needs to spawn one sandboxed child.
#[derive(Debug, Clone)]
pub struct ChildSpec {
    pub request_id: String,
    pub language: Language,
    /// Filesystem layout (pivot_root target, bind mounts, /dev-min template).
    pub mounts: Vec<MountKind>,
    /// Final argv passed to `execve`.
    pub argv: Vec<String>,
    /// Pre-filtered env (SEC-18 strip done, user env merged).
    pub env: Vec<(String, String)>,
    /// Cgroup + rlimit caps.
    pub limits: ResourceLimits,
    /// Which seccomp profile to load.
    pub seccomp_profile: SeccompProfileId,
    /// Capability subset granted (defense-in-depth — caller already filtered).
    pub layers: SandboxLayers,
    /// AppArmor profile to set via `change_onexec`.
    pub apparmor_profile: String,
    /// Verified-fd path to the cached binary, or `None` for interpreted runs.
    pub binary_fd_path: Option<String>,
    /// Wall-clock cap.
    pub wall_timeout: Duration,
    /// When true, the launcher additionally wires a private request/response
    /// pipe as fd 3 in the child (survives execve). Used by persistent RL
    /// workers: protocol frames flow over fd 3 while user stdout/stderr stay
    /// on their regular pipes. The parent keeps the read end on
    /// [`crate::ChildHandle::proto_fd`].
    pub protocol_fd: bool,
}

#[derive(Debug, Clone)]
pub enum MountKind {
    /// `tmpfs` at target with size.
    Tmpfs {
        target: PathBuf,
        size_bytes: u64,
        mode: u32,
    },
    /// Read-only bind (typically `/nix/store`).
    BindRo { src: PathBuf, target: PathBuf },
    /// Read-write bind (per-session / per-episode volumes).
    BindRw { src: PathBuf, target: PathBuf },
    /// `proc` at target with hidepid=2.
    Proc { target: PathBuf },
    /// `/dev-min` template bind (FIX SEC-10).
    DevMin { src: PathBuf, target: PathBuf },
}

#[derive(Debug, Clone, Copy)]
pub struct ResourceLimits {
    pub memory_bytes: u64,
    pub cpu_quota_us: u64,
    pub cpu_period_us: u64,
    pub pids_max: u32,
    pub nofile: u32,
    pub fsize_bytes: u64,
    pub address_space_bytes: u64,
    pub stack_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SandboxLayers {
    /// Optional: trace syscalls (settings.observability.trace_syscalls).
    pub trace_syscalls: bool,
    /// If true, MDWE (prctl PR_SET_MDWE) is enabled in addition to seccomp.
    pub enforce_w_xor_x: bool,
}

/// Logical name of a compiled seccomp BPF program. The resolver caches a
/// pre-compiled BPF blob per profile (FIX PERF-07).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SeccompProfileId {
    InterpAot,
    InterpJit,
    NativeJit,
    Go,
    RlStep,
    /// Used by `compiler` when running language compilers — relaxed
    /// (allows exec/mmap/fork) but no network.
    RelaxedCompiler,
}

impl SeccompProfileId {
    pub const fn name(self) -> &'static str {
        match self {
            Self::InterpAot => "interp-aot",
            Self::InterpJit => "interp-jit",
            Self::NativeJit => "native-jit",
            Self::Go => "go",
            Self::RlStep => "rl-step",
            Self::RelaxedCompiler => "relaxed-compiler",
        }
    }

    /// Default profile to pair with a given language. The resolver can layer
    /// capability-driven additions on top.
    pub const fn for_language(lang: Language) -> Self {
        match lang {
            Language::Python => Self::InterpAot,
            Language::Typescript => Self::InterpJit,
            Language::Go => Self::Go,
            Language::Rust | Language::Cpp => Self::NativeJit,
        }
    }
}
