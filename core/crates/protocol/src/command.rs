//! Internally-tagged enum (`cmd` discriminator) so Elixir Msgpax `%{"cmd" =>
//! "execute", ...}` maps directly without an outer wrapper.

pub use crate::settings::FileEntry;
use crate::settings::Settings;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    /// One-shot or initial run. Carries the full frozen `Settings`.
    Execute(Box<Settings>),

    /// Session-mode REPL cell. The session VM was already booted by an earlier
    /// `execute` with `mode=session`; this just hands more code in.
    ExecCell {
        id: String,
        session_id: String,
        code: String,
        #[serde(default)]
        files: Vec<FileEntry>,
        #[serde(default)]
        stdin: Option<String>,
        #[serde(default)]
        wall_ms: Option<u64>,
    },

    /// RL-mode step. Action bytes flow through; observation comes back via [`crate::Response::RlStep`].
    RlStep {
        id: String,
        episode_id: String,
        #[serde(with = "serde_bytes")]
        action: Vec<u8>,
    },

    /// Forward additional stdin to the running child.
    Stdin {
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },

    /// Interrupt the currently-running cell/step (SIGINT to PID 1).
    Interrupt { id: String },

    /// LSP request relay.
    Lsp(LspParams),

    /// Graceful shutdown - engine flushes pending events, then exits.
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspParams {
    pub method: String,
    /// Opaque msgpack bytes; the engine forwards them verbatim to the
    /// language server hosted inside the sandbox.
    #[serde(default, with = "serde_bytes")]
    pub params: Vec<u8>,
    pub req_id: u64,
}
