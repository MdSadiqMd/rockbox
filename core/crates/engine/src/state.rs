//! Engine-process state shared across mode handlers.
//!
//! Distinguishes between:
//! - Per-engine state: caches, the launcher, seccomp blobs (long-lived).
//! - Per-request state: the running child handle, drainer, output budget
//!   (per `Execute` / `ExecCell` / `RlStep`).
//!
//! Per-request state lives in `current` (only one in flight at a time per VM,
//! since the engine is single-tenant by design).

use anyhow::Result;
use cache::{BinaryCache, EnvCache};
use kernel::SandboxLauncher;
use parking_lot::Mutex;
use std::sync::Arc;

pub struct EngineState {
    pub env_cache: EnvCache,
    pub binary_cache: BinaryCache,
    /// `None` on non-Linux dev hosts; mode handlers must gracefully refuse.
    pub launcher: Option<Arc<SandboxLauncher>>,
    pub current: Mutex<Option<RequestState>>,
}

impl std::fmt::Debug for EngineState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineState")
            .field("env_cache", &self.env_cache)
            .field("binary_cache", &self.binary_cache)
            .field("launcher", &self.launcher.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct RequestState {
    pub request_id: String,
    pub stdin_writer: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    pub interrupt: Option<tokio::sync::mpsc::Sender<()>>,
}

impl EngineState {
    pub fn new() -> Self {
        // `SandboxLauncher::new()` returns `Err(Unsupported)` on non-Linux;
        // we still build the engine so the Port loop boots and unit tests
        // for non-sandbox paths (catalog, framing) work on dev hosts.
        Self {
            env_cache: EnvCache::new(16),
            binary_cache: BinaryCache::new("/var/cache/sandbox/bin"),
            launcher: SandboxLauncher::new().ok().map(Arc::new),
            current: Mutex::new(None),
        }
    }

    pub async fn send_stdin(&self, bytes: &[u8]) -> Result<()> {
        let tx = {
            let guard = self.current.lock();
            guard.as_ref().and_then(|r| r.stdin_writer.clone())
        };
        if let Some(tx) = tx {
            let _ = tx.send(bytes.to_vec()).await;
        }
        Ok(())
    }

    pub async fn interrupt(&self, _id: &str) -> Result<()> {
        let tx = {
            let guard = self.current.lock();
            guard.as_ref().and_then(|r| r.interrupt.clone())
        };
        if let Some(tx) = tx {
            let _ = tx.send(()).await;
        }
        Ok(())
    }
}
