//! Mode dispatchers. The engine picks one based on `Settings.mode`:
//!
//! - [`exec`]      — one-shot run, fresh sandbox each call
//! - [`session`]   — long-lived REPL/notebook worker (ARCH-11).
//! - [`rl`]        — RL stepper (RL-01..08)
//! - [`lsp`]       — pass-through to language server hosted inside the sandbox

use crate::data_channel::DataChannel;
use crate::state::EngineState;
use anyhow::Result;
use msgpack::FrameWriter;
use protocol::{Mode, Response, Settings};
use tokio::io::Stdout;

pub mod exec;
pub mod lsp;
pub mod rl;
pub mod session;

pub async fn dispatch(
    state: &EngineState,
    settings: Settings,
    writer: &FrameWriter<Stdout>,
    data: Option<&DataChannel>,
) -> Result<()> {
    // Any request updates the engine's active-language slot so LSP relays,
    // session cells, and RL steps that follow can look it up without every
    // command re-carrying it.
    state.set_language(settings.language);
    match settings.mode {
        Mode::Exec => exec::run(state, settings, writer, data).await,
        Mode::Session => session::start_or_attach(state, settings, writer, data).await,
        Mode::RlStep | Mode::RlEpisode => rl::start(state, settings, writer, data).await,
    }
}

/// Helper: emit an `EngineDied` response and abort the loop. Used by mode
/// handlers when something irrecoverable happens (e.g. cgroup setup failed).
pub async fn die(writer: &FrameWriter<Stdout>, reason: &str, detail: impl Into<Option<String>>) {
    let _ = writer
        .write(&Response::EngineDied(protocol::EngineDeath {
            reason: reason.into(),
            detail: detail.into(),
            exit_status: None,
            last_request_id: None,
            restart_safe: true,
        }))
        .await;
}
