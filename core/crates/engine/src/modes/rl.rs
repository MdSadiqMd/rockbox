//! RL mode - sandboxed environment worker with a reset+step protocol
//!
//! The RL contract:
//!
//! - `start` (via `Execute` with `mode = rl_step | rl_episode`) provides the
//!   env source (Python, currently the only supported language). We record
//!   the settings under the request's `request_id` as the `episode_id` and
//!   materialise the per-episode volume at
//!   `/var/lib/sandbox/episodes/<episode_id>/` (bind-mounted into the
//!   sandbox as `/episode`). The user env is expected to define
//!   `def reset()` and `def step(action_bytes)` in its entrypoint module.
//!
//! - `step` sends action bytes; the engine spawns a per-step sandbox that
//!   restores the state file, invokes `step(action)`, persists the new
//!   state, and writes the observation + reward + done to a JSON tick
//!   file. Engine reads that file and emits `Response::RlStep`.
//!
//! Performance: each step round-trip carries the ~2–6 ms sandbox spawn cost
//! (README p50) plus pickle (de)serialisation. Below-microsecond stepping
//! (the RL-01..08 stretch target) requires a persistent worker with an
//! IPC socket that survives execve, which needs launcher-side fd plumbing
//! not yet in place. This implementation is honest about that: real,
//! sandboxed, correct — but not sub-µs.

use crate::data_channel::{DataChannel, Stream};
use crate::resolver::spec as resolver_spec;
use crate::state::EngineState;
use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use msgpack::FrameWriter;
use protocol::settings::{ExtraMount, MountMode};
use protocol::{FileEntry, Language, Response, ResultStatus, Settings};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Instant;
use tokio::io::AsyncWrite;
use tracing::{debug, info, instrument};

const EPISODES_ROOT: &str = "/var/lib/sandbox/episodes";
const TICK_FILE: &str = ".rockbox_tick.json";
const ACTION_FILE: &str = ".rockbox_action.bin";
/// Raw observation bytes written by the shim (avoids a base64 round-trip
/// through the JSON tick, which cost the shim an extra import per step).
const OBS_FILE: &str = ".rockbox_obs.bin";
const SHIM_FILENAME: &str = ".rockbox_rl_shim.py";

#[instrument(skip(state, settings, writer, data), fields(req = %settings.request_id))]
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
    // Clean stale state so a re-used id starts fresh.
    for name in ["state.pkl", TICK_FILE, ACTION_FILE, OBS_FILE] {
        let _ = std::fs::remove_file(episode_dir.join(name));
    }

    // Record settings for subsequent steps to inherit.
    state
        .episodes
        .lock()
        .insert(episode_id.clone(), settings.clone());

    // Run the reset once — invokes user's `reset()` under the sandbox and
    // stashes the initial state. Any output from user code streams via the
    // usual data channel; the shim emits the initial observation to
    // OBS_FILE which we surface through the Result frame's `output` (JSON
    // payload for the caller to consume).
    let (tick, initial_obs) = match run_rl_step(state, &settings, &episode_id, None, data).await {
        Ok(t) => t,
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
            return Ok(());
        }
    };

    info!(exec_ms = %start.elapsed().as_millis(), "rl_start_done");

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
            // buffers (observation stays base64 in the JSON layer; the
            // raw bytes never cross the control channel here).
            output: {
                let mut v = serde_json::to_value(&tick).unwrap_or(serde_json::Value::Null);
                if let Some(obj) = v.as_object_mut() {
                    obj.insert(
                        "observation".into(),
                        serde_json::Value::String(
                            base64::engine::general_purpose::STANDARD.encode(&initial_obs),
                        ),
                    );
                }
                v.to_string()
            },
            errors: String::new(),
        })
        .await?;
    Ok(())
}

#[instrument(skip(state, action, writer, data), fields(req = %id, episode = %episode_id, action_bytes = action.len()))]
pub async fn step<W: AsyncWrite + Unpin>(
    state: &EngineState,
    id: String,
    episode_id: String,
    action: Vec<u8>,
    writer: &FrameWriter<W>,
    data: Option<&DataChannel>,
) -> Result<()> {
    let start = Instant::now();

    let settings = {
        let map = state.episodes.lock();
        map.get(&episode_id).cloned()
    };
    let Some(settings) = settings else {
        // No prior start — emit an "error" step with done=true so the caller
        // can unwind cleanly.
        writer
            .write(&Response::RlStep {
                request_id: id,
                episode_id,
                observation: Vec::new(),
                reward: 0.0,
                done: true,
                info: iter_info([("error", "episode not started")]),
            })
            .await?;
        return Ok(());
    };

    let t_pre = start.elapsed();
    let (tick, obs) = match run_rl_step(state, &settings, &episode_id, Some(action), data).await {
        Ok(t) => t,
        Err(e) => {
            writer
                .write(&Response::RlStep {
                    request_id: id,
                    episode_id,
                    observation: Vec::new(),
                    reward: 0.0,
                    done: true,
                    info: iter_info([("error", e.to_string().as_str())]),
                })
                .await?;
            return Ok(());
        }
    };

    info!(
        exec_ms = %start.elapsed().as_millis(),
        pre_ms = %t_pre.as_millis(),
        reward = tick.reward,
        done = tick.done,
        "rl_step_done"
    );

    writer
        .write(&Response::RlStep {
            request_id: id,
            episode_id,
            observation: obs,
            reward: tick.reward,
            done: tick.done,
            info: tick.info.unwrap_or_default(),
        })
        .await?;
    Ok(())
}

#[derive(Debug, serde::Serialize, Deserialize)]
struct Tick {
    #[serde(default)]
    reward: f64,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    info: Option<BTreeMap<String, String>>,
    #[serde(default)]
    error: Option<String>,
}

fn iter_info<'a, I: IntoIterator<Item = (&'a str, &'a str)>>(pairs: I) -> BTreeMap<String, String> {
    pairs
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

async fn run_rl_step(
    state: &EngineState,
    base: &Settings,
    episode_id: &str,
    action: Option<Vec<u8>>,
    data: Option<&DataChannel>,
) -> Result<(Tick, Vec<u8>)> {
    let t0 = Instant::now();
    if state.launcher.is_none() {
        return Err(anyhow!("sandbox launcher unavailable"));
    }
    let episode_dir = PathBuf::from(EPISODES_ROOT).join(episode_id);

    // Wire the action bytes into the episode volume BEFORE spawn — the shim
    // reads it as its first act. Absence means "reset()".
    let action_path = episode_dir.join(ACTION_FILE);
    match action {
        Some(bytes) => std::fs::write(&action_path, bytes)?,
        None => {
            let _ = std::fs::remove_file(&action_path);
        }
    }
    let tick_path = episode_dir.join(TICK_FILE);
    let obs_path = episode_dir.join(OBS_FILE);
    let _ = std::fs::remove_file(&tick_path);
    let _ = std::fs::remove_file(&obs_path);

    // Assemble a per-step Settings clone: exec mode, our shim as entrypoint,
    // user's env files preserved. Shim locates the user entrypoint via
    // ROCKBOX_USER_ENTRY env var.
    let user_entry = base.entrypoint.clone();
    let mut step_settings = base.clone();
    step_settings.mode = protocol::Mode::Exec;
    step_settings.entrypoint = SHIM_FILENAME.to_string();
    step_settings.request_id = format!("{episode_id}-tick-{}", now_millis());

    step_settings
        .env
        .insert("ROCKBOX_USER_ENTRY".into(), user_entry.clone());
    step_settings
        .env
        .insert("ROCKBOX_EPISODE_ID".into(), episode_id.to_string());

    // Guard against filename collisions and inject the shim.
    step_settings.files.retain(|f| f.path != SHIM_FILENAME);
    step_settings.files.insert(
        0,
        FileEntry {
            path: SHIM_FILENAME.to_string(),
            content: python_shim().as_bytes().to_vec(),
            mode: 0o644,
        },
    );

    if !step_settings
        .filesystem
        .mounts
        .iter()
        .any(|m| m.target == "/episode")
    {
        step_settings.filesystem.mounts.push(ExtraMount {
            source: format!("episode:{episode_id}"),
            target: "/episode".into(),
            mode: MountMode::Rw,
        });
    }

    let t_files = t0.elapsed();
    execute_and_drain(state, step_settings, data).await?;
    let t_run = t0.elapsed();

    let bytes =
        std::fs::read(&tick_path).with_context(|| format!("no tick at {}", tick_path.display()))?;
    let tick: Tick = serde_json::from_slice(&bytes).context("parse tick json")?;
    let _ = std::fs::remove_file(&tick_path);
    let _ = std::fs::remove_file(&action_path);

    // Observation arrives as raw bytes — no base64 round-trip.
    let obs = std::fs::read(&obs_path).unwrap_or_default();
    let _ = std::fs::remove_file(&obs_path);

    debug!(
        files_ms = %t_files.as_millis(),
        run_ms = %t_run.as_millis(),
        tail_ms = %t0.elapsed().as_millis() - t_run.as_millis(),
        "rl_run_step_timings"
    );

    if let Some(err) = tick.error.as_deref() {
        return Err(anyhow!("env raised: {err}"));
    }
    Ok((tick, obs))
}

async fn execute_and_drain(
    state: &EngineState,
    settings: Settings,
    data: Option<&DataChannel>,
) -> Result<()> {
    let work_root = crate::modes::work_root();
    let launcher = state
        .launcher
        .as_ref()
        .cloned()
        .ok_or_else(|| anyhow!("no launcher"))?;
    let binary_cache = state.binary_cache.clone();
    let output_bytes = settings.limits.output_bytes;
    let stream_enabled = settings.output.stream;
    let data_owned = data.cloned();

    let t0 = Instant::now();
    // Resolve, launch, and drain on ONE blocking thread — same shape as
    // exec mode. Extra hops only add scheduler latency to every step.
    let joined = {
        let launcher = launcher.clone();
        tokio::task::spawn_blocking(move || {
            let resolved = resolver_spec::resolve(&settings, work_root, Some(&binary_cache))
                .context("resolve spec")?;
            let handle = launcher.launch(&resolved.spec).context("launch")?;
            let t_launch = t0.elapsed();
            let (cg, mut drainer) = launcher
                .make_drainer(handle, output_bytes)
                .context("make_drainer")?;

            let on_stdout = |chunk: &[u8]| {
                if stream_enabled {
                    if let Some(d) = &data_owned {
                        d.send(Stream::Stdout, chunk.to_vec());
                    }
                }
            };
            let on_stderr = |chunk: &[u8]| {
                if stream_enabled {
                    if let Some(d) = &data_owned {
                        d.send(Stream::Stderr, chunk.to_vec());
                    }
                }
            };

            let exit = drainer.run(&cg, on_stdout, on_stderr).context("drain")?;
            Ok::<_, anyhow::Error>((cg, exit, t_launch))
        })
        .await
        .context("resolve/launch/drain join")??
    };
    let (cg, _child_exit, t_launch) = joined;
    let t_drain = t0.elapsed();
    launcher.release_cgroup(cg);
    let t_release = t0.elapsed();
    debug!(
        launch_ms = %t_launch.as_millis(),
        drain_ms = %t_drain.as_millis() - t_launch.as_millis(),
        release_ms = %t_release.as_millis() - t_drain.as_millis(),
        "rl_execute_and_drain"
    );
    Ok(())
}

fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_shim_contains_reset_and_step() {
        let s = python_shim();
        assert!(s.contains("reset"));
        assert!(s.contains("step"));
        assert!(s.contains("state.pkl"));
    }

    #[test]
    fn tick_parses_without_observation() {
        // Observation no longer rides in the JSON tick — it is a raw file.
        let json = br#"{"reward":1.5,"done":false,"info":{"steps":"3"}}"#;
        let tick: Tick = serde_json::from_slice(json).unwrap();
        assert!((tick.reward - 1.5).abs() < 1e-9);
        assert!(!tick.done);
        assert_eq!(
            tick.info.as_ref().unwrap().get("steps"),
            Some(&"3".to_string())
        );
        assert!(tick.error.is_none());
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
    fn shim_writes_raw_obs_file() {
        let s = python_shim();
        assert!(s.contains(OBS_FILE));
        assert!(!s.contains("base64"));
    }
}

fn python_shim() -> &'static str {
    // The shim expects the user's env module to expose either:
    //   - `reset()` → observation (bytes | bytes-like)
    //   - `step(action_bytes)` → (observation_bytes, reward: float, done: bool, info: dict[str, str])
    //
    // State persistence is opt-in: if the module also exposes `save()`
    // / `restore(state)` we shuttle a pickle through /episode/state.pkl.
    // Otherwise the module is imported fresh each tick.
    //
    // Startup-time import cost is the hot path of an RL step, so the shim
    // imports only what the happy path needs: `traceback` is imported
    // lazily inside the exception handler, and the observation is written
    // as a raw binary file (no `base64`) which the engine reads directly.
    static SHIM: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SHIM.get_or_init(|| {
        r#"import importlib.util, json, os, sys

EPISODE = "/episode"
STATE = os.path.join(EPISODE, "state.pkl")
ACTION = os.path.join(EPISODE, ".rockbox_action.bin")
TICK = os.path.join(EPISODE, ".rockbox_tick.json")
OBS = os.path.join(EPISODE, ".rockbox_obs.bin")

entry_rel = os.environ["ROCKBOX_USER_ENTRY"]
entry_path = os.path.join("/sandbox", entry_rel)

spec = importlib.util.spec_from_file_location("user_env", entry_path)
if spec is None or spec.loader is None:
    raise RuntimeError(f"cannot load user env from {entry_path}")
mod = importlib.util.module_from_spec(spec)
sys.modules["user_env"] = mod
spec.loader.exec_module(mod)

if hasattr(mod, "restore") and os.path.exists(STATE):
    try:
        import pickle
        with open(STATE, "rb") as f:
            mod.restore(pickle.load(f))
    except Exception as e:
        print(f"[rockbox] restore failed, continuing: {e}", file=sys.stderr)

def as_bytes(x):
    if isinstance(x, (bytes, bytearray)):
        return bytes(x)
    if isinstance(x, str):
        return x.encode("utf-8")
    return json.dumps(x, default=str).encode("utf-8")

def write_obs(obs):
    with open(OBS, "wb") as f:
        f.write(as_bytes(obs))

tick = {"reward": 0.0, "done": False, "info": {}}
try:
    if os.path.exists(ACTION):
        with open(ACTION, "rb") as f:
            action = f.read()
        if not hasattr(mod, "step"):
            raise RuntimeError("user env module does not define step(action)")
        result = mod.step(action)
        if isinstance(result, tuple):
            if len(result) == 4:
                obs, reward, done, info = result
            elif len(result) == 3:
                obs, reward, done = result
                info = {}
            elif len(result) == 2:
                obs, reward = result
                done, info = False, {}
            elif len(result) == 1:
                obs = result[0]
                reward, done, info = 0.0, False, {}
            else:
                raise RuntimeError(f"step returned {len(result)}-tuple, expected 1..4")
        else:
            obs = result
            reward, done, info = 0.0, False, {}
        write_obs(obs)
        tick["reward"] = float(reward)
        tick["done"] = bool(done)
        tick["info"] = {str(k): str(v) for k, v in dict(info).items()}
    else:
        if not hasattr(mod, "reset"):
            raise RuntimeError("user env module does not define reset()")
        obs = mod.reset()
        info = {}
        if isinstance(obs, tuple) and len(obs) == 2:
            obs, info = obs
        write_obs(obs)
        tick["info"] = {str(k): str(v) for k, v in dict(info).items()}
except BaseException:
    import traceback
    tick["error"] = traceback.format_exc()

if "error" not in tick and hasattr(mod, "save"):
    try:
        import pickle
        with open(STATE, "wb") as f:
            pickle.dump(mod.save(), f, protocol=pickle.HIGHEST_PROTOCOL)
    except Exception as e:
        print(f"[rockbox] save failed, continuing: {e}", file=sys.stderr)

with open(TICK, "w", encoding="utf-8") as f:
    json.dump(tick, f)

sys.stdout.flush()
sys.stderr.flush()
"#
        .to_string()
    })
}
