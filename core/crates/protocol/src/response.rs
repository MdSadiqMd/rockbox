//! Responses flowing Rust → Elixir on the control channel
//! These are raw stdout/stderr bytes do not ride this channel, they go over
//! the SOCK_SEQPACKET data channel (FIX ARCH-04). What flows here are
//! lifecycle + structured events

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    /// Engine has booted, resolved its runtime, and is ready to accept commands.
    Ready {
        language: super::Language,
        runtime: String,
        env_cached: bool,
        pid: u32,
    },

    /// Compilation started (compiled langs only).
    Compiling { request_id: String, cache_hit: bool },

    /// Compilation finished.
    Compiled {
        request_id: String,
        cached: bool,
        time_ms: u64,
    },

    /// Per-request final result for `mode=exec`.
    Result {
        request_id: String,
        status: ResultStatus,
        exit_code: i32,
        exec_time_ms: u64,
        memory_peak_mb: u64,
        cpu_time_ms: u64,
        output_bytes: u64,
        output_truncated: bool,
        /// stdout — captured here when no data channel; capped at
        /// `settings.limits.output_bytes` and truncated past that.
        #[serde(default)]
        output: String,
        errors: String,
    },

    /// RL step observation.
    ///
    /// Gymnasium-compatible: `terminated` (true env end) and `truncated`
    /// (time limit / step budget) are reported separately; `done` is kept
    /// for back-compat and equals `terminated || truncated`.
    RlStep {
        request_id: String,
        episode_id: String,
        #[serde(with = "serde_bytes")]
        observation: Vec<u8>,
        reward: f64,
        done: bool,
        #[serde(default)]
        terminated: bool,
        #[serde(default)]
        truncated: bool,
        #[serde(default)]
        info: BTreeMap<String, String>,
        /// Optional observation decoding metadata (dtype/shape/encoding),
        /// emitted by the worker when the env declares it via the `_obs_meta`
        /// info key. Lets clients decode raw obs bytes without out-of-band
        /// knowledge.
        #[serde(default)]
        obs_meta: Option<BTreeMap<String, String>>,
    },

    /// RL batched steps — one frame per `Command::RlSteps`, carrying every
    /// tick plus per-episode cumulative metrics. Order matches the request's
    /// action list; a failed step ends the batch (its error lands in that
    /// tick's `info["error"]`).
    RlSteps {
        request_id: String,
        episode_id: String,
        ticks: Vec<RlTick>,
        /// Cumulative episode metrics as of this batch: total steps taken,
        /// sum of rewards, wall time since reset.
        metrics: EpisodeMetrics,
    },

    /// Session-cell completion (state preserved in worker).
    CellResult {
        request_id: String,
        session_id: String,
        status: ResultStatus,
        exec_time_ms: u64,
        value_repr: Option<String>,
        traceback: Option<String>,
    },

    /// Periodic VM metrics (gated on settings.observability.capture_metrics).
    Metrics {
        memory_mb: u64,
        cpu_pct: f32,
        uptime_s: u64,
        rss_peak_mb: u64,
        #[serde(default)]
        gpu_mem_mb: Option<u64>,
    },

    /// LSP relay response.
    LspResponse {
        req_id: u64,
        /// Opaque msgpack bytes; orchestrator hands them straight to the client.
        #[serde(default, with = "serde_bytes")]
        result: Vec<u8>,
        #[serde(default)]
        error: Option<String>,
    },

    /// Engine is about to die (graceful or forced). Always the last frame.
    EngineDied(EngineDeath),
}

/// One tick inside a [`Response::RlSteps`] batch. Same semantics as
/// [`Response::RlStep`] minus the repeated `episode_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RlTick {
    pub request_id: String,
    #[serde(with = "serde_bytes")]
    pub observation: Vec<u8>,
    pub reward: f64,
    pub done: bool,
    #[serde(default)]
    pub terminated: bool,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub info: BTreeMap<String, String>,
    #[serde(default)]
    pub obs_meta: Option<BTreeMap<String, String>>,
}

impl RlTick {
    pub const fn empty(request_id: String) -> Self {
        Self {
            request_id,
            observation: Vec::new(),
            reward: 0.0,
            done: true,
            terminated: false,
            truncated: false,
            info: BTreeMap::new(),
            obs_meta: None,
        }
    }
}

/// Cumulative per-episode counters, updated engine-side on every exchange.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EpisodeMetrics {
    pub steps: u64,
    pub reward_sum: f64,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultStatus {
    Success,
    NonZeroExit,
    Timeout,
    OutputExceeded,
    OomKilled,
    SeccompKill,
    AppArmorDenied,
    CostExceeded,
    CompileError,
    EngineError,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineDeath {
    /// Short machine-readable category of the death, e.g. "oom", "crash", "idle_timeout"
    pub reason: String,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub exit_status: Option<i32>,
    #[serde(default)]
    pub last_request_id: Option<String>,
    /// true - if the orchestrator can safely respawn the engine
    /// false - means state may be corrupted (e.g. cgroup leak)
    pub restart_safe: bool,
}
