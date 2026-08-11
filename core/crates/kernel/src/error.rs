//! Sandbox runtime errors. Distinct from protocol errors in `protocol`.
use thiserror::Error;

pub type SandboxResult<T> = Result<T, SandboxError>;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SandboxError {
    #[error("clone3 failed: {0}")]
    Clone(String),

    #[error("uid_map / gid_map write failed: {0}")]
    UserNs(String),

    #[error("mount step failed: {step} -> {source}")]
    Mount {
        step: &'static str,
        source: std::io::Error,
    },

    #[error("seccomp install failed: {0}")]
    Seccomp(String),

    #[error("apparmor change_onexec failed: {0}")]
    AppArmor(String),

    #[error("cgroup operation failed: {op} -> {source}")]
    Cgroup {
        op: &'static str,
        source: std::io::Error,
    },

    #[error("io_uring setup failed: {0}")]
    IoUring(String),

    #[error("rlimit failed: {limit} -> {source}")]
    RLimit {
        limit: &'static str,
        source: std::io::Error,
    },

    #[error("capability drop failed: {0}")]
    CapDrop(String),

    #[error("netns template failed: {0}")]
    Net(String),

    #[error("output cap exceeded ({bytes} > {limit})")]
    OutputCap { bytes: u64, limit: u64 },

    #[error("timeout fired (wall_ms={wall_ms})")]
    Timeout { wall_ms: u64 },

    #[error("child exited with status {status}")]
    ChildExit { status: i32 },

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("operation not supported on this platform")]
    Unsupported,

    #[error("internal invariant: {0}")]
    Internal(String),
}
