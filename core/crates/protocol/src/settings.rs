//! `Settings` - the immutable, frozen request configuration produced by the
//! Elixir settings pipeline (FIX ARCH-17). The engine never re-resolves;
//! whatever the orchestrator hands over is treated as ground truth.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Top-level settings object. Versioned via [`crate::SCHEMA_VERSION`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Schema discriminator. Must equal [`crate::SCHEMA_VERSION`].
    #[serde(default = "default_schema")]
    pub schema: String,

    /// Idempotency key + tracing correlation id.
    pub request_id: String,

    /// Free-form caller labels (workspace, user, trace id). Surfaced in Prom labels.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,

    pub language: Language,

    /// Pre-baked Nix runtime (e.g. `"python-ml"`). Engine rejects unknown names.
    #[serde(default)]
    pub runtime: Option<String>,

    pub files: Vec<FileEntry>,

    /// Entrypoint path inside `/sandbox`. Defaults applied server-side.
    pub entrypoint: String,

    pub mode: Mode,

    #[serde(default)]
    pub session_id: Option<String>,

    pub limits: Limits,
    pub lifecycle: LifecycleSettings,

    #[serde(default)]
    pub capabilities: Vec<Capability>,

    #[serde(default)]
    pub env: BTreeMap<String, String>,

    /// Already resolved by SecretsBroker before the engine sees this struct.
    /// Kept here so the engine can merge into the child's env after SEC-18 strip.
    #[serde(default)]
    pub resolved_secrets: BTreeMap<String, String>,

    #[serde(default)]
    pub stdin: Option<StdinPayload>,
}

fn default_schema() -> String {
    crate::SCHEMA_VERSION.to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileEntry {
    pub path: String,
    #[serde(with = "serde_bytes")]
    pub content: Vec<u8>,
    /// Unix mode (default 0o644). The engine clamps to a safe subset.
    #[serde(default = "default_file_mode")]
    pub mode: u32,
}

const fn default_file_mode() -> u32 {
    0o644
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StdinPayload {
    Text(String),
    Bytes(#[serde(with = "serde_bytes")] Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Python,
    Typescript,
    Go,
    Rust,
    Cpp,
}

impl Language {
    pub const fn file_ext(self) -> &'static str {
        match self {
            Self::Python => "py",
            Self::Typescript => "ts",
            Self::Go => "go",
            Self::Rust => "rs",
            Self::Cpp => "cpp",
        }
    }

    pub const fn is_compiled(self) -> bool {
        matches!(self, Self::Go | Self::Rust | Self::Cpp)
    }

    pub const fn default_entrypoint(self) -> &'static str {
        match self {
            Self::Python => "main.py",
            Self::Typescript => "index.ts",
            Self::Go => "main.go",
            Self::Rust => "main.rs",
            Self::Cpp => "main.cpp",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Exec,
    Session,
    RlStep,
    RlEpisode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Concurrency,
    Subprocess,
    LargeFs,
    PersistentSession,
    Gpu,
    Install,
    RawSockets,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub wall_ms: u64,
    #[serde(default)]
    pub cpu_ms: Option<u64>,
    pub compile_ms: u64,
    pub memory_mb: u64,
    pub cpu_cores: f32,
    pub pids_max: u32,
    pub fd_max: u32,
    pub fsize_mb: u64,
    pub tmpfs_mb: u64,
    pub stack_mb: u64,
    pub output_bytes: u64,
    pub output_action: OutputAction,
    #[serde(default)]
    pub step_ms: Option<u64>,
    #[serde(default)]
    pub episode_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputAction {
    Truncate,
    KillOnOverflow,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleSettings {
    pub idle_ttl_s: u64,
    pub max_lifetime_s: u64,
    pub auto_destroy: bool,
    pub keep_alive_on_error: bool,
    pub restart_policy: LifecyclePolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecyclePolicy {
    None,
    OnOom,
    OnCrash,
    Always,
}
