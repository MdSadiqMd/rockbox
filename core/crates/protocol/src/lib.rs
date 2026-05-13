//! Wire protocol between Elixir orchestrator and Rust sandbox engine
//!
//! Transport: 4-byte big-endian length prefix + msgpack payload (see `core/crates/msgpack`)
//! Two channels per engine (FIX ARCH-04):
//! - **Control** (this crate): Elixir Port stdin/stdout, [`Command`] and [`Response`]
//! - **Data**: SOCK_SEQPACKET, raw stdout/stderr bytes batched 4ms/4KB (PERF-12)
//!
//! Versioning: every payload carries `schema: "v1"`. The engine
//! rejects unknown versions with `{type:"engine_died", reason:"settings_version_unsupported"}`

#![forbid(unsafe_code)]

pub mod command;
pub mod error;
pub mod response;
pub mod settings;

pub use command::{Command, FileEntry, LspParams};
pub use error::{ProtocolError, Result};
pub use response::{EngineDeath, Response, ResultStatus};
pub use settings::{
    Capability, CostSettings, Determinism, FilesystemSettings, GpuSettings, Language,
    LifecyclePolicy, LifecycleSettings, Limits, Mode, NetworkSettings, NetworkTier,
    ObservabilitySettings, OutputAction, OutputSettings, Settings, WebhookSettings,
};

/// Settings schema version this engine speaks
pub const SCHEMA_VERSION: &str = "v1";

/// Hard cap for any control-channel frame
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
