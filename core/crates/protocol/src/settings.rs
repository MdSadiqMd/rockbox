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

    pub network: NetworkSettings,
    pub filesystem: FilesystemSettings,

    #[serde(default)]
    pub env: BTreeMap<String, String>,

    /// Already resolved by SecretsBroker before the engine sees this struct.
    /// Kept here so the engine can merge into the child's env after SEC-18 strip.
    #[serde(default)]
    pub resolved_secrets: BTreeMap<String, String>,

    #[serde(default)]
    pub stdin: Option<StdinPayload>,

    #[serde(default)]
    pub determinism: Determinism,

    #[serde(default)]
    pub gpu: GpuSettings,

    pub output: OutputSettings,
    pub observability: ObservabilitySettings,

    #[serde(default)]
    pub cost: CostSettings,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkSettings {
    pub tier: NetworkTier,
    #[serde(default)]
    pub allowlist: Option<Vec<String>>,
    #[serde(default)]
    pub bandwidth_mbps: Option<u32>,
    #[serde(default)]
    pub max_conns: Option<u32>,
    /// Always true, caller cannot disable. Field exists so the wire format
    /// is explicit about the guarantee.
    pub block_metadata: bool,
    pub dns: DnsSettings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NetworkTier {
    None,
    Loopback,
    EgressAllowlist,
    EgressOpen,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DnsSettings {
    pub mode: DnsMode,
    pub cache_s: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DnsMode {
    Proxied,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilesystemSettings {
    #[serde(default)]
    pub writable_paths: Vec<String>,
    #[serde(default)]
    pub mounts: Vec<ExtraMount>,
    #[serde(default)]
    pub preserve_on_exit: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtraMount {
    /// Source URI: `session:<uuid>`, `episode:<uuid>`, `volume:<id>`.
    pub source: String,
    pub target: String,
    pub mode: MountMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MountMode {
    Ro,
    Rw,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Determinism {
    #[serde(default)]
    pub seed: Option<u64>,
    #[serde(default)]
    pub deterministic_time: bool,
    #[serde(default)]
    pub freeze_random: bool,
    #[serde(default)]
    pub pin_runtime_hash: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GpuSettings {
    #[serde(default)]
    pub count: u32,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub memory_mb: Option<u32>,
    #[serde(default)]
    pub mig: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputSettings {
    pub stream: bool,
    pub binary_safe: bool,
    pub merge_streams: bool,
    pub include_metrics: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservabilitySettings {
    pub capture_metrics: bool,
    #[serde(default)]
    pub trace_syscalls: bool,
    #[serde(default)]
    pub webhook: Option<WebhookSettings>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookSettings {
    pub url: String,
    #[serde(default)]
    pub events: Vec<String>,
    /// HMAC key already resolved by SecretsBroker.
    #[serde(default)]
    pub hmac_key: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostSettings {
    #[serde(default)]
    pub max_credits: Option<u64>,
    /// 0.0..=1.0 fraction of `max_credits` at which to alert.
    #[serde(default)]
    pub alert_at: Option<f32>,
}

impl Settings {
    /// Cheap structural validation. Heavier cross-field checks happen in the
    /// Elixir SettingsValidator before the engine ever sees this; this is the
    /// last-line defense if the Port channel is tampered with.
    pub fn validate(&self) -> Result<(), crate::ProtocolError> {
        use crate::ProtocolError as E;
        if self.schema != crate::SCHEMA_VERSION {
            return Err(E::UnsupportedSchema {
                version: self.schema.clone(),
                expected: crate::SCHEMA_VERSION,
            });
        }
        if self.request_id.is_empty() {
            return Err(E::InvalidRequestId("empty".into()));
        }
        if self.files.is_empty() {
            return Err(E::InvariantViolation("files[] is empty".into()));
        }
        if self.limits.memory_mb == 0 {
            return Err(E::InvariantViolation("memory_mb=0".into()));
        }
        if matches!(self.mode, Mode::Session) && self.session_id.is_none() {
            return Err(E::InvariantViolation(
                "mode=session needs session_id".into(),
            ));
        }
        if self.network.tier == NetworkTier::None
            && self.capabilities.contains(&Capability::Install)
        {
            return Err(E::InvariantViolation(
                "+install requires network>=allowlist".into(),
            ));
        }
        Ok(())
    }

    pub fn has_capability(&self, cap: Capability) -> bool {
        self.capabilities.contains(&cap)
    }
}
