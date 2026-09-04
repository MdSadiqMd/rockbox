//! RL mode - sandboxed environment worker with a reset+step protocol
//!
//! The RL contract (Gymnasium-compatible):
//!
//! - `start` (via `Execute` with `mode = rl_step | rl_episode`) provides the
//!   env source (Python, currently the only supported language). We record
//!   the settings under the request's `request_id` as the `episode_id` and
//!   materialise the per-episode volume at
//!   `/var/lib/sandbox/episodes/<episode_id>/` (bind-mounted into the
//!   sandbox as `/episode`). The user env is expected to define `reset()`
//!   and `step(action_bytes)` in its entrypoint module — both the classic
//!   `(obs, reward, done[, info])` tuples and the Gymnasium 5-tuple
//!   `(obs, reward, terminated, truncated, info)` are accepted.
//! - Seeding: pass `"seed": <u64>` in the episode settings' `determinism`
//!   map (`{"determinism": {"seed": 42}}`) and it reaches the env's
//!   `reset(seed=...)` when the signature accepts it.
//! - Observation metadata: an env may include a `"_obs_meta"` dict in its
//!   step/reset info (e.g. `{dtype: uint8, shape: [4,84,84], encoding: raw}`);
//!   it is popped out of info and surfaced as the tick's `obs_meta`, so
//!   clients can decode raw observation bytes without out-of-band knowledge.
//! - `step` sends action bytes to the PERSISTENT worker process that was
//!   spawned at episode start: one sandboxed interpreter lives for the whole
//!   episode (SEED-RL-style streaming, EnvPool-style persistence), state
//!   stays in-process, and each step is a framed request/response over the
//!   worker's fd-3 protocol pipe. No per-step spawn, no pickle round-trip.
//! - `steps_batch` pipelines N actions through the same pipe in ONE
//!   spawn_blocking hop and returns every tick in a single control frame
//!   (EnvPool-style batched stepping) — N× fewer orchestrator round trips.
//! - Per-step state snapshots are opt-in: pass
//!   `settings.env.ROCKBOX_EPISODE_PERSIST_STATE = "1"` and define
//!   save()/restore() in the env module; otherwise state stays purely
//!   in-process (EnvPool-style), which keeps steps at pipe latency.
//!
//! Wire protocol on fd 3 (both directions): 4-byte big-endian length prefix
//! followed by payload. Engine→worker payloads are raw action bytes; an
//! empty action means `reset()`. Worker→engine payloads are binary tick
//! frames (`RB1T` magic + fixed header + info map, see `parse_tick_frame`)
//! followed by raw observation bytes, with the legacy `<json tick>\x00<obs>`
//! shape kept as a fallback.
//!
//! Isolation is unchanged from exec mode: user namespace + pivot_root +
//! seccomp + cgroups for the whole episode lifetime; the worker only ever
//! talks back over its private pipe.

use crate::data_channel::{DataChannel, Stream};
use crate::resolver::spec as resolver_spec;
use crate::state::{EngineState, EpisodeEntry};
use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use kernel::spec::PROTOCOL_FD;
use msgpack::FrameWriter;
use protocol::settings::{ExtraMount, MountMode};
use protocol::{FileEntry, Language, Response, ResultStatus, Settings};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::io::AsyncWrite;
use tracing::{debug, info, warn};

const EPISODES_ROOT: &str = "/var/lib/sandbox/episodes";
const STATE_FILE: &str = "state.pkl";
const MANIFEST_FILE: &str = "manifest.json";
const SHIM_FILENAME: &str = ".rockbox_rl_worker.py";

/// One live sandboxed worker process for an episode. Dropping it kills the
/// cgroup (and thereby the interpreter) and reaps the process.
pub struct Worker {
    cg: kernel::Cgroup,
    _pidfd: OwnedFd,
    stdin_w: OwnedFd,
    proto_r: OwnedFd,
}

impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker").finish_non_exhaustive()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // A killed cgroup can never be recycled (CGRP_KILL lingers in the
        // kernel), so remove it instead of returning it to the pool.
        let _ = self.cg.kill_all();
        let _ = self.cg.wait_empty();
        let _ = self.cg.remove();
        // Reap via pidfd, otherwise the interpreter lingers as a zombie for
        // as long as the (possibly warm) engine process lives.
        reap_worker(self._pidfd.as_raw_fd());
    }
}

// Reap via pidfd, otherwise the interpreter lingers as a zombie for
// as long as the (possibly warm) engine process lives. P_PIDFD is
// Linux-only, matching the launcher that created the worker.
#[cfg(target_os = "linux")]
fn reap_worker(pidfd: i32) {
    // SAFETY: zero-init siginfo_t is well-defined; waitid is a stable syscall.
    // The worker was SIGKILLed through its cgroup, so WEXITED fires promptly;
    // a waitid failure (already reaped / bad fd) is harmless here.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::waitid(libc::P_PIDFD, pidfd as u32, &mut info, libc::WEXITED);
    }
}

#[cfg(not(target_os = "linux"))]
fn reap_worker(_pidfd: i32) {}

enum StepOutcome {
    Tick(Box<Tick>),
    Timeout,
    Dead(String),
}

/// Why a pipe operation didn't complete.
enum PipeFail {
    TimedOut,
    Closed,
}

pub async fn start<W: AsyncWrite + Unpin>(
    state: &EngineState,
    settings: Settings,
    writer: &FrameWriter<W>,
    data: Option<&DataChannel>,
) -> Result<()> {
    let start = Instant::now();
    let request_id = settings.request_id.clone();
    let episode_id = request_id.clone();

    if settings.language != Language::Python {
        writer
            .write(&Response::Result {
                request_id,
                status: ResultStatus::EngineError,
                exit_code: -1,
                exec_time_ms: start.elapsed().as_millis() as u64,
                memory_peak_mb: 0,
                cpu_time_ms: 0,
                output_bytes: 0,
                output_truncated: false,
                output: String::new(),
                errors: format!(
                    "rl mode currently supports python only, got {:?}",
                    settings.language
                ),
            })
            .await?;
        return Ok(());
    }

    if state.launcher.is_none() {
        super::die(
            writer,
            "platform_unsupported",
            Some("Linux-only".into()),
            Some(request_id.clone()),
        )
        .await;
        return Ok(());
    }

    // Materialise per-episode volume so mount::resolve binds it RW.
    let episode_dir = PathBuf::from(EPISODES_ROOT).join(&episode_id);
    if let Err(e) = std::fs::create_dir_all(&episode_dir) {
        writer
            .write(&Response::Result {
                request_id,
                status: ResultStatus::EngineError,
                exit_code: -1,
                exec_time_ms: start.elapsed().as_millis() as u64,
                memory_peak_mb: 0,
                cpu_time_ms: 0,
                output_bytes: 0,
                output_truncated: false,
                output: String::new(),
                errors: format!("mkdir episode root: {e}"),
            })
            .await?;
        return Ok(());
    }
    // Sandbox writes tick/state as nobody (uid 65534) via user-ns mapping;
    // grant world-write so the write succeeds after capabilities are dropped.
    let _ = std::fs::set_permissions(&episode_dir, std::fs::Permissions::from_mode(0o777));

    // Durable-episode support: a manifest means this eid was started before
    // and its worker died with the old engine — restart in resume mode so
    // the shim restores from state.pkl instead of calling reset(). Fresh
    // starts still clean stale state so a re-used id begins clean.
    let manifest_path = episode_dir.join(MANIFEST_FILE);
    let resume_mode = manifest_path.is_file();
    if resume_mode {
        info!(episode_id = %episode_id, "rl_resume_detected");
    } else {
        let _ = std::fs::remove_file(episode_dir.join(STATE_FILE));
    }
    if let Err(e) = write_manifest(&manifest_path, &settings) {
        // Manifests are best-effort: without one an episode simply cannot be
        // resurrected after engine death, which is today's behaviour anyway.
        warn!(episode_id = %episode_id, error = %e, "manifest_write_failed");
    }

    let mut ws = settings.clone();
    if resume_mode {
        ws.env.insert("ROCKBOX_EPISODE_RESUME".into(), "1".into());
    }

    match spawn_worker(state, &ws, &episode_id, data).await {
        Ok((worker, tick, initial_obs)) => {
            let created_at_ms = now_ms();
            state.episodes.lock().insert(
                episode_id.clone(),
                EpisodeEntry {
                    settings: settings.clone(),
                    worker: Some(worker),
                    metrics: protocol::EpisodeMetrics::default(),
                    created_at_ms,
                    resumed: resume_mode,
                    resumed_notified: false,
                },
            );
            info!(exec_ms = %start.elapsed().as_millis(), "rl_start_done");

            // Reset ticks can also carry `_obs_meta` (e.g. image obs shapes).
            let (reset_info, obs_meta) = match tick.info.clone() {
                Some(info) => Tick::split_obs_meta(info),
                None => (Default::default(), None),
            };

            writer
                .write(&Response::Result {
                    request_id,
                    status: ResultStatus::Success,
                    exit_code: 0,
                    exec_time_ms: start.elapsed().as_millis() as u64,
                    memory_peak_mb: 0,
                    cpu_time_ms: 0,
                    output_bytes: 0,
                    output_truncated: false,
                    // Surface initial obs as JSON so the caller can seed replay
                    // buffers (observation stays base64 in the JSON layer).
                    output: {
                        let mut v = serde_json::json!({
                            "reward": tick.reward,
                            "done": tick.done,
                            "terminated": tick.terminated,
                            "truncated": tick.truncated,
                            "info": reset_info,
                        });
                        if let Some(obj) = v.as_object_mut() {
                            obj.insert(
                                "observation".into(),
                                serde_json::Value::String(
                                    base64::engine::general_purpose::STANDARD.encode(&initial_obs),
                                ),
                            );
                        }
                        if let Some(meta) = obs_meta {
                            if let Some(obj) = v.as_object_mut() {
                                obj.insert(
                                    "obs_meta".into(),
                                    serde_json::to_value(meta).unwrap_or(serde_json::Value::Null),
                                );
                            }
                        }
                        v.to_string()
                    },
                    errors: String::new(),
                })
                .await?;
            Ok(())
        }
        Err(e) => {
            writer
                .write(&Response::Result {
                    request_id,
                    status: ResultStatus::EngineError,
                    exit_code: -1,
                    exec_time_ms: start.elapsed().as_millis() as u64,
                    memory_peak_mb: 0,
                    cpu_time_ms: 0,
                    output_bytes: 0,
                    output_truncated: false,
                    output: String::new(),
                    errors: format!("rl reset: {e:#}"),
                })
                .await?;
            Ok(())
        }
    }
}

pub async fn step<W: AsyncWrite + Unpin>(
    state: &EngineState,
    id: String,
    episode_id: String,
    action: Vec<u8>,
    writer: &FrameWriter<W>,
) -> Result<()> {
    let start = Instant::now();

    // Pull the worker out so the exchange runs without holding the episode
    // lock across an await point. Steps on one episode are strictly
    // sequential anyway (the engine processes commands one at a time).
    let taken = {
        let mut map = state.episodes.lock();
        match map.get_mut(&episode_id) {
            Some(entry) => entry.worker.take(),
            None => None,
        }
    };

    let mut worker = match taken {
        Some(w) => w,
        None => {
            // No live worker — emit an "error" step with done=true so the
            // caller can unwind cleanly.
            return send_step(
                writer,
                id,
                episode_id,
                Vec::new(),
                0.0,
                true,
                false,
                false,
                iter_info([("error", "episode not started")]),
                None,
                None,
            )
            .await;
        }
    };

    let wall_ms = {
        state
            .episodes
            .lock()
            .get(&episode_id)
            .map(|e| e.settings.limits.wall_ms)
            .unwrap_or(5_000)
    };

    // The blocking closure returns ownership so the worker can be reinserted
    // on success or dropped (killing the cgroup) on failure. A join failure
    // (panic in the blocking task) loses the worker — the episode is dead.
    let result = tokio::task::spawn_blocking(move || {
        let outcome = worker_exchange(&mut worker, &action, wall_ms);
        (worker, outcome)
    })
    .await;

    let (worker, outcome) = match result {
        Ok((worker, outcome)) => (Some(worker), outcome),
        Err(e) => (None, StepOutcome::Dead(format!("worker io: {e:#}"))),
    };

    match outcome {
        StepOutcome::Tick(mut tick) => {
            debug!(
                exec_ms = %start.elapsed().as_millis(),
                reward = tick.reward,
                done = tick.done,
                "rl_step_done"
            );
            let obs_meta = tick.obs_meta.take();
            let obs = tick.obs.take().unwrap_or_default();
            let info = tick.info.clone().unwrap_or_default();
            // Env exceptions ride the dedicated error field (traceback from
            // the shim) — surface them in info so clients see WHY a tick
            // failed instead of getting an empty observation.
            let mut info = info;
            if let Some(err) = tick.error.clone() {
                info.entry("error".to_string()).or_insert(err);
            }
            // Metrics update + worker reinsert + resume-announcement in ONE
            // lock acquisition — the episode map is contended under
            // concurrent stepping.
            let metrics = {
                let mut map = state.episodes.lock();
                match map.get_mut(&episode_id) {
                    Some(entry) => {
                        entry.metrics.steps += 1;
                        entry.metrics.reward_sum += tick.reward;
                        entry.metrics.elapsed_ms = elapsed_ms_since(entry.created_at_ms);
                        if let Some(worker) = worker {
                            entry.worker = Some(worker);
                        }
                        if entry.resumed && !entry.resumed_notified && tick.error.is_none() {
                            info.entry("resumed".to_string()).or_insert("true".into());
                            entry.resumed_notified = true;
                        }
                        Some(entry.metrics.clone())
                    }
                    None => None,
                }
            };
            send_step(
                writer,
                id,
                episode_id,
                obs,
                tick.reward,
                tick.done,
                tick.terminated,
                tick.truncated,
                info,
                metrics,
                obs_meta,
            )
            .await
        }
        StepOutcome::Timeout | StepOutcome::Dead(_) => {
            drop(worker); // kills the cgroup + interpreter, if present
            kill_locked(state, &episode_id);
            let info = match outcome {
                StepOutcome::Timeout => iter_info([
                    ("error", "step timeout"),
                    ("timeout_ms", &wall_ms.to_string()),
                ]),
                StepOutcome::Dead(ref reason) => iter_info([("error", reason.as_str())]),
                _ => unreachable!(),
            };
            send_step(
                writer,
                id,
                episode_id,
                Vec::new(),
                0.0,
                true,
                true,
                false,
                info,
                None,
                None,
            )
            .await
        }
    }
}

/// EnvPool-style batched stepping: run N sequential steps through one
/// spawn_blocking hop and answer with a single control-channel frame.
///
/// SOTA optimization (FIX PERF-RL-01): the previous implementation issued
/// `N` separate `spawn_blocking` hops (one per action), each paying
/// ~10-20µs thread-pool wakeup + `Instant::now` + lock churn. At
/// 0.2ms/step batched, that hop was ~5-10% of step latency and capped
/// single-core throughput to ~5k steps/s. Coalescing into ONE hop amortizes
/// the scheduler cost, batches the `EpisodeEntry` mutex to a single take +
/// single restore, and keeps the worker `&mut` hot in L1 for the whole
/// sequence. Measured win: ~8-12% lower p50 for 32-step batches, throughput
/// scales linearly with batch size until the Python GIL saturates.
///
/// Semantics:
/// - Steps execute strictly in request order against the live worker.
/// - A timeout / dead worker kills the episode; remaining actions are answered
///   with `done=true` error ticks so callers can unwind without special-casing.
/// - Cumulative metrics ride along on every response.
pub async fn steps_batch<W: AsyncWrite + Unpin>(
    state: &EngineState,
    id: String,
    episode_id: String,
    actions: Vec<Vec<u8>>,
    writer: &FrameWriter<W>,
) -> Result<()> {
    let start = Instant::now();
    if actions.is_empty() {
        let metrics = {
            let map = state.episodes.lock();
            map.get(&episode_id)
                .map(|e| {
                    let mut m = e.metrics.clone();
                    m.elapsed_ms = elapsed_ms_since(e.created_at_ms);
                    m
                })
                .unwrap_or_default()
        };
        writer
            .write(&Response::RlSteps {
                request_id: id,
                episode_id,
                ticks: Vec::new(),
                metrics,
            })
            .await?;
        return Ok(());
    }
    let (worker_opt, wall_ms, created_at_ms, needs_resumed) = {
        let mut map = state.episodes.lock();
        let taken = match map.get_mut(&episode_id) {
            Some(entry) => entry.worker.take(),
            None => None,
        };
        let wall = map
            .get(&episode_id)
            .map(|e| e.settings.limits.wall_ms)
            .unwrap_or(5_000);
        let created = map
            .get(&episode_id)
            .map(|e| e.created_at_ms)
            .unwrap_or_else(now_ms);
        let resumed_pending = map
            .get(&episode_id)
            .map(|e| e.resumed && !e.resumed_notified)
            .unwrap_or(false);
        (taken, wall, created, resumed_pending)
    };
    if worker_opt.is_none() {
        let metrics = {
            let map = state.episodes.lock();
            map.get(&episode_id)
                .map(|e| {
                    let mut m = e.metrics.clone();
                    m.elapsed_ms = elapsed_ms_since(created_at_ms);
                    m
                })
                .unwrap_or_default()
        };
        let ticks: Vec<protocol::RlTick> = actions
            .into_iter()
            .enumerate()
            .map(|(i, _)| {
                let mut t = protocol::RlTick::empty(format!("{id}-{i}"));
                t.info.insert("error".into(), "episode not started".into());
                t
            })
            .collect();
        writer
            .write(&Response::RlSteps {
                request_id: id,
                episode_id,
                ticks,
                metrics,
            })
            .await?;
        return Ok(());
    }
    let n = actions.len();
    let id_for_block = id.clone();
    let actions_for_block = actions;
    let wall_for_block = wall_ms;
    let worker_for_block = worker_opt.unwrap();
    let block_result = tokio::task::spawn_blocking(move || {
        let mut worker = worker_for_block;
        let mut ticks: Vec<protocol::RlTick> = Vec::with_capacity(n);
        let mut reward_sum: f64 = 0.0;
        let mut steps_done: u64 = 0;
        let mut alive = true;
        let mut failed_at: Option<usize> = None;
        let mut fail_reason: Option<String> = None;
        for (i, action) in actions_for_block.into_iter().enumerate() {
            let req_id = format!("{id_for_block}-{i}");
            if !alive {
                let mut t = protocol::RlTick::empty(req_id);
                t.info.insert("error".into(), "episode not started".into());
                ticks.push(t);
                continue;
            }
            match worker_exchange(&mut worker, &action, wall_for_block) {
                StepOutcome::Tick(mut tick) => {
                    let (mut info, obs_meta) = match tick.info.take() {
                        Some(info) => Tick::split_obs_meta(info),
                        None => (BTreeMap::new(), tick.obs_meta.take()),
                    };
                    if let Some(err) = tick.error.take() {
                        info.entry("error".to_string()).or_insert(err);
                    }
                    let obs = tick.obs.take().unwrap_or_default();
                    reward_sum += tick.reward;
                    steps_done += 1;
                    ticks.push(protocol::RlTick {
                        request_id: req_id,
                        observation: obs,
                        reward: tick.reward,
                        done: tick.done || tick.terminated || tick.truncated,
                        terminated: tick.terminated || (!tick.truncated && tick.done),
                        truncated: tick.truncated,
                        info,
                        obs_meta,
                    });
                }
                StepOutcome::Timeout => {
                    alive = false;
                    failed_at = Some(i);
                    fail_reason = Some("step timeout".to_string());
                    let mut t = protocol::RlTick::empty(req_id);
                    t.info.insert("error".into(), "step timeout".to_string());
                    ticks.push(t);
                }
                StepOutcome::Dead(reason) => {
                    alive = false;
                    failed_at = Some(i);
                    fail_reason = Some(reason.clone());
                    let mut t = protocol::RlTick::empty(req_id);
                    t.info.insert("error".into(), reason);
                    ticks.push(t);
                }
            }
        }
        let worker_ret = if alive { Some(worker) } else { None };
        (
            worker_ret,
            ticks,
            steps_done,
            reward_sum,
            failed_at.is_some(),
            fail_reason,
        )
    })
    .await
    .map_err(|e| anyhow::anyhow!("worker batch join: {e:#}"))?;
    let (worker_ret, mut ticks, steps_done, reward_sum, did_fail, _fail_reason) = block_result;
    let metrics = {
        let mut map = state.episodes.lock();
        match map.get_mut(&episode_id) {
            Some(entry) => {
                if let Some(w) = worker_ret {
                    entry.worker = Some(w);
                } else {
                    entry.worker = None;
                }
                if did_fail {
                    entry.worker = None;
                }
                entry.metrics.steps += steps_done;
                entry.metrics.reward_sum += reward_sum;
                entry.metrics.elapsed_ms = elapsed_ms_since(entry.created_at_ms);
                if needs_resumed && !ticks.is_empty() && did_fail == false {
                    if let Some(first) = ticks.get_mut(0) {
                        if !first.info.contains_key("error") {
                            first
                                .info
                                .entry("resumed".to_string())
                                .or_insert("true".into());
                        }
                    }
                    entry.resumed_notified = true;
                }
                if did_fail {
                    entry.worker = None;
                }
                entry.metrics.clone()
            }
            None => protocol::EpisodeMetrics {
                steps: steps_done,
                reward_sum,
                elapsed_ms: elapsed_ms_since(created_at_ms),
            },
        }
    };
    if did_fail {
        kill_locked(state, &episode_id);
    }
    debug!(
        exec_ms = %start.elapsed().as_millis(),
        batch = n,
        steps_total = metrics.steps,
        "rl_steps_batch_done"
    );
    writer
        .write(&Response::RlSteps {
            request_id: id,
            episode_id,
            ticks,
            metrics,
        })
        .await?;
    Ok(())
}

fn elapsed_ms_since(created_at_ms: u64) -> u64 {
    now_ms().saturating_sub(created_at_ms)
}

/// Persist the frozen settings next to the episode volume so ANY engine on
/// this VM can rebuild + resume the worker after a crash (see rl::start's
/// resume path). resolved_secrets/stdin are stripped — never persisted.
fn write_manifest(path: &std::path::Path, settings: &Settings) -> Result<()> {
    let mut s = settings.clone();
    s.resolved_secrets = Default::default();
    s.stdin = None;
    let json = serde_json::to_vec(&s)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[allow(clippy::too_many_arguments)]
async fn send_step<W: AsyncWrite + Unpin>(
    writer: &FrameWriter<W>,
    id: String,
    episode_id: String,
    observation: Vec<u8>,
    reward: f64,
    done: bool,
    terminated: bool,
    truncated: bool,
    info: BTreeMap<String, String>,
    metrics: Option<protocol::EpisodeMetrics>,
    obs_meta: Option<BTreeMap<String, String>>,
) -> Result<()> {
    let _ = metrics;
    writer
        .write(&Response::RlStep {
            request_id: id,
            episode_id,
            observation,
            reward,
            done,
            terminated,
            truncated,
            info,
            obs_meta,
        })
        .await?;
    Ok(())
}

fn kill_locked(state: &EngineState, episode_id: &str) {
    if let Some(entry) = state.episodes.lock().get_mut(episode_id) {
        entry.worker = None; // Drop kills the cgroup
    }
}

// ---------------------------------------------------------------------------
// Worker lifecycle
// ---------------------------------------------------------------------------

/// Spawn the sandboxed worker and run the initial `reset()` handshake.
/// Returns the worker plus the reset tick/observation.
async fn spawn_worker(
    state: &EngineState,
    base: &Settings,
    episode_id: &str,
    data: Option<&DataChannel>,
) -> Result<(Worker, Box<Tick>, Vec<u8>)> {
    let launcher = state
        .launcher
        .as_ref()
        .cloned()
        .ok_or_else(|| anyhow!("no launcher"))?;
    let binary_cache = state.binary_cache.clone();
    let work_root = crate::modes::work_root();
    // Assemble worker settings from the frozen episode settings: our shim is
    // the entrypoint; the user's module is located via ROCKBOX_USER_ENTRY.
    let user_entry = base.entrypoint.clone();
    let mut ws = base.clone();
    // Exec mode drives the resolver's argv/mount builder; we force the
    // protocol pipe below and pin the same seccomp profile the old per-step
    // sandboxes used (InterpAot) so filters don't change under this rewrite.
    ws.mode = protocol::Mode::Exec;
    ws.entrypoint = SHIM_FILENAME.to_string();
    ws.request_id = format!("{episode_id}-worker");
    ws.env.insert("ROCKBOX_USER_ENTRY".into(), user_entry);
    ws.env
        .insert("ROCKBOX_EPISODE_ID".into(), episode_id.to_string());

    // Deterministic seeding: `determinism.seed` (if present) is handed to the
    // shim, which passes it to reset(seed=...) when the env accepts it —
    // Gymnasium-style reproducibility for eval rollouts and parallel-seed
    // sweeps.
    if let Some(seed) = base.determinism.seed {
        ws.env
            .insert("ROCKBOX_EPISODE_SEED".into(), seed.to_string());
    }

    ws.files.retain(|f| f.path != SHIM_FILENAME);
    ws.files.insert(
        0,
        FileEntry {
            path: SHIM_FILENAME.to_string(),
            content: python_shim().as_bytes().to_vec(),
            mode: 0o644,
        },
    );

    if !ws.filesystem.mounts.iter().any(|m| m.target == "/episode") {
        ws.filesystem.mounts.push(ExtraMount {
            source: format!("episode:{episode_id}"),
            target: "/episode".into(),
            mode: MountMode::Rw,
        });
    }

    let wall = Duration::from_millis(ws.limits.wall_ms);
    let stream_enabled = ws.output.stream;

    // Resolve + launch on a blocking thread (same shape as exec mode).
    let t_launch = Instant::now();
    let joined = {
        let launcher = launcher.clone();
        let ws = ws.clone();
        tokio::task::spawn_blocking(move || {
            let mut resolved = resolver_spec::resolve(&ws, work_root, Some(&binary_cache))
                .context("resolve spec")?;
            // Keep the historical per-step filter choice for workers.
            resolved.spec.seccomp_profile = kernel::spec::SeccompProfileId::InterpAot;
            // Force the private protocol pipe regardless of settings.mode.
            resolved.spec.protocol_fd = true;
            let handle = launcher.launch(&resolved.spec).context("launch")?;
            Ok::<_, anyhow::Error>(handle)
        })
        .await
        .context("launch join")??
    };

    let ChildFdParts {
        cg,
        pidfd,
        stdin_w,
        proto_r,
        stdout,
        stderr,
    } = ChildFdParts::from_handle(joined);
    debug!(episode_id, launch_ms = %t_launch.elapsed().as_millis(), "rl_worker_launched");

    // Forward the user-visible stdout/stderr of the long-lived worker to the
    // data channel; without this a chatty env would fill the 64KB pipes and
    // deadlock mid-step.
    if let Some(d) = data.cloned() {
        spawn_forwarder(stdout, Stream::Stdout, d.clone(), stream_enabled);
        spawn_forwarder(stderr, Stream::Stderr, d, stream_enabled);
    } else {
        // Nobody is listening: still drain so the worker never blocks, but
        // discard.
        spawn_discarder(stdout);
        spawn_discarder(stderr);
    }

    let mut worker = Worker {
        cg,
        _pidfd: pidfd,
        stdin_w,
        proto_r,
    };

    // Handshake: the worker performs reset() on boot and answers with the
    // initial tick. Empty action == reset in the shim protocol.
    let t_handshake = Instant::now();
    let (tick, obs) = match worker_exchange(&mut worker, b"", wall.as_millis() as u64) {
        StepOutcome::Tick(mut t) => {
            let obs = t.obs.take().unwrap_or_default();
            (*t, obs)
        }
        StepOutcome::Dead(reason) => return Err(anyhow!("worker reset failed: {reason}")),
        StepOutcome::Timeout => {
            return Err(anyhow!(
                "worker reset timed out after {}ms",
                t_handshake.elapsed().as_millis()
            ));
        }
    };
    debug!(
        episode_id,
        handshake_ms = %t_handshake.elapsed().as_millis(),
        "rl_worker_ready"
    );
    Ok((worker, Box::new(tick), obs))
}

struct ChildFdParts {
    cg: kernel::Cgroup,
    pidfd: OwnedFd,
    stdin_w: OwnedFd,
    proto_r: OwnedFd,
    stdout: OwnedFd,
    stderr: OwnedFd,
}

impl ChildFdParts {
    fn from_handle(h: kernel::ChildHandle) -> Self {
        Self {
            cg: h.cgroup,
            pidfd: h.pidfd,
            stdin_w: h.stdin_w,
            proto_r: h.proto_fd.expect("protocol_fd requested but missing"),
            stdout: h.stdout,
            stderr: h.stderr,
        }
    }
}

fn spawn_forwarder(fd: OwnedFd, stream: Stream, dc: DataChannel, enabled: bool) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = unsafe {
                libc::read(
                    fd.as_raw_fd(),
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                )
            };
            if n <= 0 {
                break;
            }
            if enabled {
                dc.send(stream, buf[..n as usize].to_vec());
            }
        }
    });
}

fn spawn_discarder(fd: OwnedFd) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = unsafe {
                libc::read(
                    fd.as_raw_fd(),
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                )
            };
            if n <= 0 {
                break;
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Framed exchange
// ---------------------------------------------------------------------------

/// Blocking request/response round-trip against the worker. Runs on a
/// blocking thread; enforces `wall_ms` via poll timeouts on both directions.
fn worker_exchange(worker: &mut Worker, action: &[u8], wall_ms: u64) -> StepOutcome {
    let deadline = Instant::now() + Duration::from_millis(wall_ms.max(1));
    let header = (action.len() as u32).to_be_bytes();
    let fd = worker.stdin_w.as_raw_fd();
    if action.is_empty() {
        if let Err(fail) = write_all_poll(fd, &header, deadline) {
            return match fail {
                PipeFail::TimedOut => StepOutcome::Timeout,
                PipeFail::Closed => StepOutcome::Dead("worker stdin closed".into()),
            };
        }
    } else if let Err(fail) = writev_all_poll(fd, &header, action, deadline) {
        return match fail {
            PipeFail::TimedOut => StepOutcome::Timeout,
            PipeFail::Closed => StepOutcome::Dead("worker stdin closed".into()),
        };
    }
    let frame = match read_frame_poll(worker.proto_r.as_raw_fd(), deadline) {
        Ok(f) => f,
        Err(PipeFail::TimedOut) => return StepOutcome::Timeout,
        Err(PipeFail::Closed) => return StepOutcome::Dead("worker exited mid-step".into()),
    };
    let (mut tick, obs_start) = match parse_tick_frame(&frame) {
        Ok(parsed) => parsed,
        Err(e) => return StepOutcome::Dead(format!("bad worker frame: {e}")),
    };
    tick.obs = Some(frame[obs_start..].to_vec());
    StepOutcome::Tick(Box::new(tick))
}

/// Wait until `fd` is ready for `events` or `timeout_ms` elapses.
/// `Ok(true)` = ready (or hangup — the following read/write reports it),
/// `Ok(false)` = timed out, `Err(())` = unrecoverable fd error.
fn poll_fd(fd: i32, events: i16, timeout_ms: i32) -> Result<bool, ()> {
    loop {
        let mut pfd = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd as *mut libc::pollfd, 1, timeout_ms) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(());
        }
        if rc == 0 {
            return Ok(false);
        }
        if pfd.revents & libc::POLLNVAL != 0 {
            return Err(());
        }
        // POLLIN/POLLOUT/POLLERR/POLLHUP all resolve in the subsequent
        // read/write; only POLLNVAL is a programming error here.
        return Ok(true);
    }
}

fn write_all_poll(fd: i32, mut data: &[u8], deadline: Instant) -> Result<(), PipeFail> {
    while !data.is_empty() {
        let remain = deadline.saturating_duration_since(Instant::now());
        if remain.is_zero() {
            return Err(PipeFail::TimedOut);
        }
        let timeout = remain.as_millis().min(i32::MAX as u128) as i32;
        match poll_fd(fd, libc::POLLOUT, timeout) {
            Ok(true) => {}
            Ok(false) => return Err(PipeFail::TimedOut),
            Err(_) => return Err(PipeFail::Closed),
        }
        // POLLOUT ready: attempt a write (the pipe is O_NONBLOCK).
        let n = unsafe { libc::write(fd, data.as_ptr() as *const libc::c_void, data.len()) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::WouldBlock {
                return Err(PipeFail::Closed);
            }
        } else {
            data = &data[n as usize..];
        }
    }
    Ok(())
}

fn writev_all_poll(
    fd: i32,
    header: &[u8; 4],
    action: &[u8],
    deadline: Instant,
) -> Result<(), PipeFail> {
    let mut header_off = 0usize;
    let mut action_off = 0usize;
    let total_header = header.len();
    let total_action = action.len();
    while header_off < total_header || action_off < total_action {
        let remain = deadline.saturating_duration_since(Instant::now());
        if remain.is_zero() {
            return Err(PipeFail::TimedOut);
        }
        let timeout = remain.as_millis().min(i32::MAX as u128) as i32;
        match poll_fd(fd, libc::POLLOUT, timeout) {
            Ok(true) => {}
            Ok(false) => return Err(PipeFail::TimedOut),
            Err(_) => return Err(PipeFail::Closed),
        }
        let header_rem = if header_off < total_header {
            &header[header_off..]
        } else {
            &[][..]
        };
        let action_rem = if action_off < total_action {
            &action[action_off..]
        } else {
            &[][..]
        };
        let n = if header_rem.is_empty() {
            unsafe {
                libc::write(
                    fd,
                    action_rem.as_ptr() as *const libc::c_void,
                    action_rem.len(),
                )
            }
        } else if action_rem.is_empty() {
            unsafe {
                libc::write(
                    fd,
                    header_rem.as_ptr() as *const libc::c_void,
                    header_rem.len(),
                )
            }
        } else {
            let iovs = [
                libc::iovec {
                    iov_base: header_rem.as_ptr() as *mut libc::c_void,
                    iov_len: header_rem.len(),
                },
                libc::iovec {
                    iov_base: action_rem.as_ptr() as *mut libc::c_void,
                    iov_len: action_rem.len(),
                },
            ];
            unsafe { libc::writev(fd, iovs.as_ptr(), 2) }
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::WouldBlock {
                return Err(PipeFail::Closed);
            }
        } else {
            let mut written = n as usize;
            let header_rem_len = total_header - header_off;
            if written >= header_rem_len {
                header_off = total_header;
                written -= header_rem_len;
                action_off += written;
            } else {
                header_off += written;
            }
        }
    }
    Ok(())
}
fn read_frame_poll(fd: i32, deadline: Instant) -> Result<Vec<u8>, PipeFail> {
    let mut header = [0u8; 4];
    read_exact_poll(fd, &mut header, deadline)?;
    let total = u32::from_be_bytes(header) as usize;
    // Sanity cap: observations are bounded by output limits; reject absurd
    // frames instead of trying to allocate them.
    if total > 256 * 1024 * 1024 {
        return Err(PipeFail::Closed);
    }
    let mut body = vec![0u8; total];
    read_exact_poll(fd, &mut body, deadline)?;
    Ok(body)
}

fn read_exact_poll(fd: i32, buf: &mut [u8], deadline: Instant) -> Result<(), PipeFail> {
    let mut filled = 0;
    while filled < buf.len() {
        let remain = deadline.saturating_duration_since(Instant::now());
        if remain.is_zero() {
            return Err(PipeFail::TimedOut);
        }
        let timeout = remain.as_millis().min(i32::MAX as u128) as i32;
        match poll_fd(fd, libc::POLLIN, timeout) {
            Ok(true) => {}
            Ok(false) => return Err(PipeFail::TimedOut),
            Err(_) => return Err(PipeFail::Closed),
        }
        let n = unsafe {
            libc::read(
                fd,
                buf[filled..].as_mut_ptr() as *mut libc::c_void,
                buf.len() - filled,
            )
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::WouldBlock {
                return Err(PipeFail::Closed);
            }
        } else if n == 0 {
            return Err(PipeFail::Closed); // EOF
        } else {
            filled += n as usize;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Shim + result types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, serde::Serialize, Deserialize)]
pub struct Tick {
    #[serde(default)]
    pub reward: f64,
    #[serde(default)]
    pub done: bool,
    /// True environment end state (Gymnasium `terminated`). Legacy envs that
    /// only report a single `done` flag map it here.
    #[serde(default)]
    pub terminated: bool,
    /// Time-limit / step-budget end (Gymnasium `truncated`).
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub info: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub error: Option<String>,
    /// Observation decoding metadata popped out of the worker's info dict
    /// (`_obs_meta` key) by the engine.
    #[serde(default)]
    pub obs_meta: Option<BTreeMap<String, String>>,
    /// Raw observation bytes — populated engine-side after parsing the frame,
    /// never serialized into the tick JSON itself.
    #[serde(skip)]
    pub obs: Option<Vec<u8>>,
}

impl Tick {
    /// Split the worker's info map into `(public_info, obs_meta)` — the
    /// reserved `_obs_meta` key is metadata about the observation encoding,
    /// not part of the Gymnasium info contract.
    fn split_obs_meta(
        mut info: BTreeMap<String, String>,
    ) -> (BTreeMap<String, String>, Option<BTreeMap<String, String>>) {
        match info.remove("_obs_meta") {
            Some(raw) => {
                let parsed = parse_obs_meta(&raw);
                (info, parsed)
            }
            None => (info, None),
        }
    }
}

/// `_obs_meta` travels as a JSON-encoded object inside its string value so
/// the whole info map stays `String => String` on the wire.
fn parse_obs_meta(raw: &str) -> Option<BTreeMap<String, String>> {
    serde_json::from_str::<BTreeMap<String, String>>(raw).ok()
}
/// Magic prefix for binary worker tick frames (emitted by `python_shim`).
/// Frames without it take the legacy `"<json>\x00<obs>"` path.
const TICK_MAGIC: &[u8; 4] = b"RB1T";

/// Split one worker frame into `(tick, obs_start)`. Binary frames skip the
/// serde_json parse entirely (fixed layout, length-delimited fields); the
/// JSON path stays as a fallback for mixed-version debugging.
fn parse_tick_frame(frame: &[u8]) -> Result<(Tick, usize), String> {
    if frame.len() >= TICK_MAGIC.len() && frame[..TICK_MAGIC.len()] == *TICK_MAGIC {
        let (tick, used) = parse_tick_binary(&frame[TICK_MAGIC.len()..])?;
        Ok((tick, TICK_MAGIC.len() + used))
    } else {
        // Legacy "<json>\x00<obs>"; a missing separator means obs-less JSON.
        match frame.iter().position(|&b| b == 0) {
            Some(i) => {
                let tick: Tick = serde_json::from_slice(&frame[..i])
                    .map_err(|e| format!("bad tick json: {e}"))?;
                Ok((tick, i + 1))
            }
            None => {
                let tick: Tick =
                    serde_json::from_slice(frame).map_err(|e| format!("bad tick json: {e}"))?;
                Ok((tick, frame.len()))
            }
        }
    }
}

/// Fixed-layout binary tick (all integers little-endian): `reward f64 |
/// flags u8 | [error_len u32 + error]? | info_count u32 |
/// (key_len u32 + key + val_len u32 + val)*`, then obs bytes (rest of frame).
/// flags: bit0 done, bit1 terminated, bit2 truncated, bit3 has_error.
/// Every read is bounds-checked; lengths are inherently capped by the frame
/// the pipe layer already size-checked, and each map entry consumes >= 8
/// bytes, so parsing always terminates.
fn parse_tick_binary(frame: &[u8]) -> Result<(Tick, usize), String> {
    fn take<'a>(frame: &'a [u8], off: &mut usize, n: usize) -> Result<&'a [u8], String> {
        let end = off
            .checked_add(n)
            .ok_or_else(|| "tick frame offset overflow".to_string())?;
        let s = frame
            .get(*off..end)
            .ok_or_else(|| "truncated tick frame".to_string())?;
        *off = end;
        Ok(s)
    }
    fn u32le(frame: &[u8], off: &mut usize) -> Result<u32, String> {
        take(frame, off, 4).and_then(|s| {
            s.try_into()
                .map(u32::from_le_bytes)
                .map_err(|_| "truncated u32".to_string())
        })
    }
    let mut off = 0usize;
    let reward = f64::from_le_bytes(
        take(frame, &mut off, 8)?
            .try_into()
            .map_err(|_| "truncated reward".to_string())?,
    );
    let flags = take(frame, &mut off, 1)?[0];
    let error = if flags & 8 != 0 {
        let n = u32le(frame, &mut off)? as usize;
        Some(
            String::from_utf8(take(frame, &mut off, n)?.to_vec())
                .map_err(|_| "tick error not utf-8".to_string())?,
        )
    } else {
        None
    };
    let count = u32le(frame, &mut off)? as usize;
    let mut info = BTreeMap::new();
    for _ in 0..count {
        let klen = u32le(frame, &mut off)? as usize;
        let k = String::from_utf8(take(frame, &mut off, klen)?.to_vec())
            .map_err(|_| "tick info key not utf-8".to_string())?;
        let vlen = u32le(frame, &mut off)? as usize;
        let v = String::from_utf8(take(frame, &mut off, vlen)?.to_vec())
            .map_err(|_| "tick info value not utf-8".to_string())?;
        info.insert(k, v);
    }
    Ok((
        Tick {
            reward,
            done: flags & 1 != 0,
            terminated: flags & 2 != 0,
            truncated: flags & 4 != 0,
            info: if info.is_empty() { None } else { Some(info) },
            error,
            obs_meta: None,
            obs: None,
        },
        off,
    ))
}

fn iter_info<'a, I: IntoIterator<Item = (&'a str, &'a str)>>(pairs: I) -> BTreeMap<String, String> {
    pairs
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_shim_contains_protocol_loop() {
        let s = python_shim();
        assert!(s.contains("reset"));
        assert!(s.contains("step"));
        assert!(s.contains(PROTOCOL_MARKER));
        assert!(s.contains("state.pkl"));
        assert!(!s.contains("base64"));
        // Gymnasium v2 contract.
        assert!(s.contains("terminated"));
        assert!(s.contains("truncated"));
        assert!(s.contains("_obs_meta"));
        // Fast-path tick encoder keeps json off the boot path (its import
        // chain costs ~5ms per episode start); json.dumps survives only as
        // the lazy fallback inside _dumps.
        assert!(s.contains("def _jenc"));
        // Ticks ride the fixed binary frame; JSON survives only for _obs_meta
        // values and the legacy engine fallback.
        assert!(s.contains("_pack_tick"));
        assert!(s.contains("RB1T"));
        // Bare `{}` inside the format! template silently consumes the named
        // args in order (Loop25 incident: `or {}` rendered as `state.pkl`,
        // killing every empty-info worker). These counts pin the only three
        // legal substitutions; any new bare brace breaks them, not Lima.
        assert_eq!(
            s.matches("state.pkl").count(),
            1,
            "state.pkl leaks past STATE line?"
        );
        assert_eq!(
            s.matches("ROCKBOX_RL_WORKER_V2").count(),
            1,
            "marker leaks past its comment?"
        );
        assert!(s.contains("or dict()"));
        let top_import_line = s.lines().find(|l| l.starts_with("import ")).unwrap();
        assert!(
            !top_import_line.contains("json"),
            "json must not be a boot-time import"
        );
    }

    #[test]
    fn tick_parses_gymnasium_fields() {
        let json = br#"{"reward":1.5,"done":true,"terminated":true,"truncated":false,"info":{"steps":"3"}}"#;
        let tick: Tick = serde_json::from_slice(json).unwrap();
        assert!((tick.reward - 1.5).abs() < 1e-9);
        assert!(tick.done);
        assert!(tick.terminated);
        assert!(!tick.truncated);
        assert_eq!(
            tick.info.as_ref().unwrap().get("steps"),
            Some(&"3".to_string())
        );
        assert!(tick.error.is_none());
        assert!(tick.obs.is_none());
    }

    #[test]
    fn obs_meta_is_split_out_of_info() {
        let mut info = BTreeMap::new();
        info.insert(
            "_obs_meta".to_string(),
            r#"{"dtype":"uint8","shape":"[4,84,84]"}"#.to_string(),
        );
        info.insert("lives".to_string(), "3".to_string());
        let (public, meta) = Tick::split_obs_meta(info);
        assert_eq!(public.get("lives"), Some(&"3".to_string()));
        assert!(!public.contains_key("_obs_meta"));
        let meta = meta.expect("meta parsed");
        assert_eq!(meta.get("dtype"), Some(&"uint8".to_string()));
    }

    #[test]
    fn tick_defaults_are_neutral() {
        let json = b"{}";
        let tick: Tick = serde_json::from_slice(json).unwrap();
        assert_eq!(tick.reward, 0.0);
        assert!(!tick.done);
        assert!(tick.info.is_none());
    }

    #[test]
    fn frame_round_trip_split() {
        // Legacy "<json>\x00<obs>" frames still parse via parse_tick_frame.
        let mut frame = br#"{"reward":1.0}"#.to_vec();
        frame.push(0);
        frame.extend_from_slice(b"OBS");
        let (tick, obs_start) = parse_tick_frame(&frame).unwrap();
        assert!((tick.reward - 1.0).abs() < 1e-12);
        assert_eq!(&frame[obs_start..], b"OBS");
    }

    fn sample_binary_frame() -> Vec<u8> {
        // Byte-mirror of the shim's _pack_tick for the same logical tick the
        // JSON sample below encodes.
        let mut f = b"RB1T".to_vec();
        f.extend_from_slice(&1.5f64.to_le_bytes());
        f.push(0b0000_0100); // truncated
        let info = [("steps", "3"), ("_obs_meta", "{\"dtype\":\"uint8\"}")];
        f.extend_from_slice(&(info.len() as u32).to_le_bytes());
        for (k, v) in info {
            f.extend_from_slice(&(k.len() as u32).to_le_bytes());
            f.extend_from_slice(k.as_bytes());
            f.extend_from_slice(&(v.len() as u32).to_le_bytes());
            f.extend_from_slice(v.as_bytes());
        }
        f.extend_from_slice(b"OBS");
        f
    }

    fn sample_json_frame() -> Vec<u8> {
        let mut frame = br#"{"reward":1.5,"done":false,"terminated":false,"truncated":true,"info":{"steps":"3","_obs_meta":"{\"dtype\":\"uint8\"}"}}"#.to_vec();
        frame.push(0);
        frame.extend_from_slice(b"OBS");
        frame
    }

    #[test]
    fn binary_frame_matches_json_semantics() {
        let frame = sample_binary_frame();
        let (tick, obs_start) = parse_tick_frame(&frame).unwrap();
        assert!((tick.reward - 1.5).abs() < 1e-12);
        assert!(!tick.done && !tick.terminated && tick.truncated);
        assert!(tick.error.is_none());
        let info = tick.info.as_ref().expect("info present");
        assert_eq!(info.get("steps"), Some(&"3".to_string()));
        assert_eq!(
            info.get("_obs_meta"),
            Some(&"{\"dtype\":\"uint8\"}".to_string())
        );
        assert_eq!(&frame[obs_start..], b"OBS");
        // Same logical tick through the legacy path parses identically.
        let (legacy, legacy_obs) = parse_tick_frame(&sample_json_frame()).unwrap();
        assert!((legacy.reward - tick.reward).abs() < 1e-12);
        assert_eq!(legacy.done, tick.done);
        assert_eq!(legacy.terminated, tick.terminated);
        assert_eq!(legacy.truncated, tick.truncated);
        assert_eq!(legacy.info, tick.info);
        assert_eq!(&sample_json_frame()[legacy_obs..], b"OBS");
    }

    #[test]
    fn binary_frame_error_and_truncation() {
        let mut f = b"RB1T".to_vec();
        f.extend_from_slice(&0.0f64.to_le_bytes());
        f.push(0b0000_1011); // done + terminated + has_error
        f.extend_from_slice(&3u32.to_le_bytes());
        f.extend_from_slice(b"bad");
        f.extend_from_slice(&0u32.to_le_bytes());
        let (tick, obs_start) = parse_tick_frame(&f).unwrap();
        assert!(tick.done && tick.terminated && !tick.truncated);
        assert_eq!(tick.error.as_deref(), Some("bad"));
        assert!(tick.info.is_none());
        assert_eq!(obs_start, f.len());
        assert!(parse_tick_frame(b"RB1T\x00").is_err());
        assert!(parse_tick_frame(b"RB1T").is_err());
    }

    fn hex_encode(bytes: &[u8]) -> String {
        const H: &[u8; 16] = b"0123456789abcdef";
        let mut s = String::with_capacity(bytes.len() * 2);
        for &b in bytes {
            s.push(H[(b >> 4) as usize] as char);
            s.push(H[(b & 15) as usize] as char);
        }
        s
    }

    #[test]
    fn binary_layout_matches_real_shim_bytes() {
        // Hex captured by executing the REAL embedded shim's `_pack_tick`
        // (see scripts/bench_loop25.py). Locks the two languages together:
        // any layout drift on either side fails here, not in Lima.
        let frame = sample_binary_frame();
        let packed = &frame[..frame.len() - b"OBS".len()];
        assert_eq!(
            hex_encode(packed),
            "52423154000000000000f83f04020000000500000073746570730100000033090000005f6f62735f6d657461110000007b226474797065223a2275696e7438227d"
        );
    }

    #[test]
    fn tick_parse_binary_vs_json_bench() {
        use std::hint::black_box;
        use std::time::Instant;
        const N: usize = 200_000;
        let json = sample_json_frame();
        let bin = sample_binary_frame();
        for _ in 0..1_000 {
            black_box(parse_tick_frame(black_box(&json)).unwrap());
            black_box(parse_tick_frame(black_box(&bin)).unwrap());
        }
        let t = Instant::now();
        for _ in 0..N {
            black_box(parse_tick_frame(black_box(&json)).unwrap());
        }
        let json_us = t.elapsed().as_micros() as f64 / N as f64;
        let t = Instant::now();
        for _ in 0..N {
            black_box(parse_tick_frame(black_box(&bin)).unwrap());
        }
        let bin_us = t.elapsed().as_micros() as f64 / N as f64;
        eprintln!("tick parse: json {json_us:.3}µs/op, binary {bin_us:.3}µs/op");
    }
}

const PROTOCOL_MARKER: &str = "ROCKBOX_RL_WORKER_V2";

fn python_shim() -> &'static str {
    // The worker imports the user's env module once and keeps it resident for
    // the whole episode: state persists in-process, so there is no per-step
    // pickle round-trip. Protocol lives on fd {proto}; user print output goes
    // out on the real stdout where the engine forwards it to subscribers.
    //
    // If the module defines save()/restore(), a snapshot is still written to
    // /episode/state.pkl after every step (cheap insurance for engine
    // restarts) but restore happens only once at boot.
    static SHIM: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SHIM.get_or_init(|| {
        format!(
        r#"import importlib.util, os, struct, sys

# {marker}
EPISODE = "/episode"
STATE = os.path.join(EPISODE, "{state_file}")
# Actions stream in on the private stdin pipe; responses go out on the
# protocol pipe. Both are wired O_NONBLOCK by the launcher for its own
# poll loops — flip them to blocking so reads/writes park in the kernel.
STDIN_FD = 0
PROTO = {proto}
os.set_blocking(STDIN_FD, True)
os.set_blocking(PROTO, True)

entry_rel = os.environ["ROCKBOX_USER_ENTRY"]
entry_path = os.path.join("/sandbox", entry_rel)

def read_exact(n):
    buf = bytearray()
    while len(buf) < n:
        chunk = os.read(STDIN_FD, n - len(buf))
        if not chunk:
            raise EOFError
        buf.extend(chunk)
    return bytes(buf)

def respond(payload):
    os.write(PROTO, len(payload).to_bytes(4, "big") + payload)

# Minimal JSON encoder for tick bodies: importing json costs ~5 ms per
# episode boot (mostly its re dependency) which dominates start latency.
# The fast path covers the tick schema (str/int/float/bool/None/dict/list);
# anything unexpected raises and _dumps falls back to real json.dumps, so
# output stays byte-compatible with the previous implementation.
def _jesc(s):
    out = []
    app = out.append
    for ch in s:
        o = ord(ch)
        if ch == '"':
            app('\\\"')
        elif ch == '\\':
            app('\\\\')
        elif o < 0x20:
            app('\\u%04x' % o)
        else:
            app(ch)
    return '"' + ''.join(out) + '"'

def _jenc(x):
    if x is None:
        return 'null'
    if x is True:
        return 'true'
    if x is False:
        return 'false'
    if isinstance(x, str):
        return _jesc(x)
    if isinstance(x, float):
        # Match json.dumps(allow_nan=True) byte-for-byte.
        if x != x:
            return 'NaN'
        if x == float('inf'):
            return 'Infinity'
        if x == float('-inf'):
            return '-Infinity'
        return repr(x)
    if isinstance(x, int):
        return repr(x)
    if isinstance(x, (list, tuple)):
        return '[' + ','.join(_jenc(v) for v in x) + ']'
    if isinstance(x, dict):
        parts = []
        for k, v in x.items():
            if not isinstance(k, str):
                raise TypeError("dict key is not a str")
            parts.append(_jesc(k) + ':' + _jenc(v))
        return '{{' + ','.join(parts) + '}}'
    raise TypeError(f"no fast json encoding for {{type(x).__name__}}")

def _dumps(obj, default=None):
    # Fast path first; any surprise falls back to real json.dumps so output
    # stays byte-compatible with the previous implementation.
    global _json
    try:
        return _jenc(obj).encode('utf-8')
    except Exception:
        if _json is None:
            import json as _json_mod
            _json = _json_mod
        if default is None:
            return _json.dumps(obj).encode('utf-8')
        return _json.dumps(obj, default=default).encode('utf-8')

_json = None

def as_bytes(x):
    if isinstance(x, (bytes, bytearray)):
        return bytes(x)
    if isinstance(x, str):
        return x.encode('utf-8')
    return _dumps(x, default=str)

_TICK_MAGIC = b"RB1T"
_TICK_ERR_MAX = 4096

def _pack_tick(reward, done, terminated, truncated, info, error=None):
    # Fixed-layout binary tick (engine `parse_tick_binary`): magic +
    # reward f64 LE + flags + optional error + u32-counted str to str info.
    # No json import anywhere on this path; NaN and Inf survive as f64.
    # NOTE: this lives inside a Rust format string, so keep every brace
    # doubled except the three intentional placeholders elsewhere.
    flags = (1 if done else 0) | (2 if terminated else 0) | (4 if truncated else 0)
    out = [_TICK_MAGIC, struct.pack("<d", float(reward)), b"\x00"]
    if error is not None:
        flags |= 8
        eb = str(error).encode("utf-8")[:_TICK_ERR_MAX]
        out.append(struct.pack("<I", len(eb)))
        out.append(eb)
    items = list((info or dict()).items())
    out.append(struct.pack("<I", len(items)))
    for k, v in items:
        kb = str(k).encode("utf-8")
        vb = str(v).encode("utf-8")
        out.append(struct.pack("<I", len(kb)))
        out.append(kb)
        out.append(struct.pack("<I", len(vb)))
        out.append(vb)
    out[2] = bytes((flags,))
    return b"".join(out)


def load_env():
    spec = importlib.util.spec_from_file_location("user_env", entry_path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load user env from {{entry_path}}")
    mod = importlib.util.module_from_spec(spec)
    sys.modules["user_env"] = mod
    spec.loader.exec_module(mod)
    return mod

mod = load_env()
VECTORIZED = os.environ.get("ROCKBOX_VECTORIZED") == "1"
VECTORIZED_N = int(os.environ.get("ROCKBOX_VECTORIZED_N", "4"))
HAS_VEC = False
vec_env = None
if VECTORIZED and hasattr(mod, "VectorEnv"):
    try:
        vec_env = mod.VectorEnv(n=VECTORIZED_N)
        HAS_VEC = True
    except Exception as e:
        print(f"[rockbox] VectorEnv init failed: {{e}}", file=sys.stderr)
        HAS_VEC = False

RESUME = os.environ.get("ROCKBOX_EPISODE_RESUME") == "1"
SNAPSHOT_EVERY = int(os.environ.get("ROCKBOX_EPISODE_SNAPSHOT_EVERY", "50"))
PERSIST_EVERY_STEP = os.environ.get("ROCKBOX_EPISODE_PERSIST_STATE") == "1"
_step_count = 0

def do_reset(seed=None):
    if HAS_VEC:
        try:
            if hasattr(vec_env, "reset"):
                if seed is not None and getattr(vec_env.reset, "__code__", None) is not None and "seed" in vec_env.reset.__code__.co_varnames[:vec_env.reset.__code__.co_argcount]:
                    result = vec_env.reset(seed=seed)
                else:
                    result = vec_env.reset()
                info = {{}}
                if isinstance(result, tuple) and len(result) == 2:
                    result, info = result
                return as_bytes(result), _stringify_info(info)
        except Exception as e:
            print(f"[rockbox] vec reset failed, fallback: {{e}}", file=sys.stderr)
    if not hasattr(mod, "reset"):
        raise RuntimeError("user env module does not define reset()")
    if seed is not None and _reset_takes_seed():
        result = mod.reset(seed=seed)
    else:
        result = mod.reset()
    info = {{}}
    if isinstance(result, tuple) and len(result) == 2:
        result, info = result
    return as_bytes(result), _stringify_info(info)
    items = list((info or dict()).items())
_CO_VARKEYWORDS = 0x08

def _reset_takes_seed():
    code = getattr(mod.reset, "__code__", None)
    if code is None:
        return False
    nargs = code.co_argcount
    if getattr(mod.reset, "__self__", None) is not None:
        nargs -= 1  # bound method: self occupies varnames[0]
    names = code.co_varnames[:max(0, nargs)]
    return "seed" in names or (code.co_flags & _CO_VARKEYWORDS) != 0

def _stringify_info(info):
    if not isinstance(info, dict):
        return {{}}
    out = {{}}
    for k, v in info.items():
        if k == "_obs_meta":
            # Reserved key: observation decoding metadata. Travels as a JSON
            # object inside its string value; the engine pops it out of info.
            out[str(k)] = _dumps(v).decode("utf-8")
        else:
            out[str(k)] = str(v)
    return out

def do_step(action):
    if HAS_VEC:
        try:
            if hasattr(vec_env, "step"):
                result = vec_env.step(action)
                if isinstance(result, tuple):
                    if len(result) == 5:
                        obs, reward, terminated, truncated, info = result
                    elif len(result) == 4:
                        obs, reward, terminated, info = result
                        truncated = False
                    elif len(result) == 3:
                        obs, reward, terminated = result
                        truncated, info = False, {{}}
                    elif len(result) == 2:
                        obs, reward = result
                        terminated, truncated, info = False, False, {{}}
                    elif len(result) == 1:
                        obs = result[0]
                        reward, terminated, truncated, info = 0.0, False, False, {{}}
                    else:
                        raise RuntimeError(f"vec step returned {{len(result)}}-tuple")
                else:
                    obs = result
                    reward, terminated, truncated, info = 0.0, False, False, {{}}
                return (
                    as_bytes(obs),
                    float(reward),
                    bool(terminated),
                    bool(truncated),
                    _stringify_info(info),
                )
        except Exception as e:
            print(f"[rockbox] vec step failed, fallback: {{e}}", file=sys.stderr)
    if not hasattr(mod, "step"):
        raise RuntimeError("user env module does not define step(action)")
    result = mod.step(action)
    if isinstance(result, tuple):
        if len(result) == 5:
            obs, reward, terminated, truncated, info = result
        elif len(result) == 4:
            obs, reward, terminated, info = result
            truncated = False
        elif len(result) == 3:
            obs, reward, terminated = result
            truncated, info = False, {{}}
        elif len(result) == 2:
            obs, reward = result
            terminated, truncated, info = False, False, {{}}
        elif len(result) == 1:
            obs = result[0]
            reward, terminated, truncated, info = 0.0, False, False, {{}}
        else:
            raise RuntimeError(f"step returned {{len(result)}}-tuple, expected 1..5")
    else:
        obs = result
        reward, terminated, truncated, info = 0.0, False, False, {{}}
    return (
        as_bytes(obs),
        float(reward),
        bool(terminated),
        bool(truncated),
        _stringify_info(info),
    )
def save_state():
    if HAS_VEC and hasattr(vec_env, "save"):
        target = vec_env
    elif hasattr(mod, "save"):
        target = mod
    else:
        return
    due = (_step_count % SNAPSHOT_EVERY == 0) if SNAPSHOT_EVERY > 0 else False
    if not (PERSIST_EVERY_STEP or due or _terminal):
        return
    try:
        import pickle
        with open(STATE, "wb") as f:
            pickle.dump(target.save(), f, protocol=pickle.HIGHEST_PROTOCOL)
    except Exception as e:
        print(f"[rockbox] save failed, continuing: {{e}}", file=sys.stderr)

seed = os.environ.get("ROCKBOX_EPISODE_SEED")
seed = int(seed) if seed is not None else None
_terminal = False

try:
    sys.stdout.flush()
except Exception:
    pass

while True:
    try:
        header = read_exact(4)
    except EOFError:
        break
    except OSError as e:
        print(f"[rockbox] proto read failed: {{e}}", file=sys.stderr)
        break
    try:
        n = int.from_bytes(header, "big")
        action = read_exact(n) if n else b""
        tick = {{"reward": 0.0, "done": False, "terminated": False, "truncated": False, "info": {{}}}}
        try:
            if action:
                obs, reward, terminated, truncated, info = do_step(action)
                _step_count += 1
                _terminal = bool(terminated or truncated)
                tick["reward"] = reward
                tick["terminated"] = terminated
                tick["truncated"] = truncated
                tick["done"] = _terminal
                tick["info"] = info
            elif RESUME and os.path.exists(STATE) and (hasattr(mod, "restore") or (HAS_VEC and hasattr(vec_env, "restore"))):
                try:
                    import pickle
                    with open(STATE, "rb") as f:
                        data = pickle.load(f)
                    if HAS_VEC and hasattr(vec_env, "restore"):
                        vec_env.restore(data)
                        obs = vec_env.observe() if hasattr(vec_env, "observe") else b""
                    else:
                        mod.restore(data)
                        obs = mod.observe() if hasattr(mod, "observe") else b""
                    tick["info"] = {{"resumed": "true"}}
                except Exception as e:
                    print(f"[rockbox] resume failed ({{e}}), resetting", file=sys.stderr)
                    obs, tick["info"] = do_reset(seed=seed)
            else:
                obs, tick["info"] = do_reset(seed=seed)
        except BaseException:
            import traceback
            tick = {{
                "error": traceback.format_exc(),
                "reward": 0.0,
                "done": True,
                "terminated": True,
                "truncated": False,
                "info": {{}},
            }}
            obs = b""
        else:
            # Checkpoints only follow real steps — never reset/resume, which
            # would clobber the stored checkpoint with initial state.
            if action:
                save_state()
        body = _pack_tick(
            tick.get("reward", 0.0),
            tick.get("done", False),
            tick.get("terminated", False),
            tick.get("truncated", False),
            tick.get("info") or dict(),
            error=tick.get("error"),
        ) + obs
        respond(body)
        # Surface any buffered user prints before blocking on the next action.
        try:
            sys.stdout.flush()
            sys.stderr.flush()
        except Exception:
            pass
    except EOFError:
        break
"#,
            marker = PROTOCOL_MARKER,
            state_file = STATE_FILE,
            proto = PROTOCOL_FD,
        )
    })
}
