//! Session mode - namespace-preserving REPL semantics via disk-backed
//! checkpoints (ARCH-11).
//!
//! Design tradeoffs vs. an in-process REPL:
//!
//! - Cost: each cell reboots the interpreter (Python cold-start ≈ 5–15 ms
//!   warm, ≈ 50–120 ms cold) plus checkpoint (de)serialisation.
//! - Benefit: reuses every layer of the exec-mode sandbox (10-layer stack,
//!   cgroup limits, output cap, seccomp, drainer) without any launcher
//!   surgery. Session state is a byte-for-byte pickle/JSON of the previous
//!   global scope — no wire-protocol changes, no fd plumbing, no ambient
//!   runner process to babysit.
//!
//! Lifecycle:
//!
//! 1. `start_or_attach` — records the initial `Settings` under `session_id`,
//!    materialises `/var/lib/sandbox/sessions/<sid>/`, sends a Ready signal.
//! 2. `run_cell` — clones the stored settings, injects a per-language shim
//!    as the entrypoint, appends the user cell as an aux file, delegates to
//!    `modes::exec`. The shim restores state, executes the cell, persists
//!    state, and writes a structured result under
//!    `/session/.rockbox_cell_result.json`. Engine reads that file after
//!    the child exits and emits a `CellResult` frame.
//! 3. Subsequent cells with the same `session_id` re-enter step 2; state
//!    accumulates on disk between calls.

use crate::data_channel::{DataChannel, Stream};
use crate::resolver::spec as resolver_spec;
use crate::state::EngineState;
use anyhow::{Context, Result, anyhow};
use msgpack::FrameWriter;
use protocol::settings::{ExtraMount, MountMode};
use protocol::{Capability, FileEntry, Language, Response, ResultStatus, Settings};
use serde::Deserialize;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Instant;
use tokio::io::AsyncWrite;
use tracing::{info, instrument, warn};

const SESSIONS_ROOT: &str = "/var/lib/sandbox/sessions";
const CELL_RESULT_FILE: &str = ".rockbox_cell_result.json";
const SHIM_FILENAME_PY: &str = ".rockbox_session_shim.py";
const SHIM_FILENAME_JS: &str = ".rockbox_session_shim.mjs";
const CELL_FILENAME: &str = ".rockbox_cell";

#[instrument(skip(state, settings, writer, _data), fields(req = %settings.request_id))]
pub async fn start_or_attach<W: AsyncWrite + Unpin>(
    state: &EngineState,
    settings: Settings,
    writer: &FrameWriter<W>,
    _data: Option<&DataChannel>,
) -> Result<()> {
    let session_id = settings
        .session_id
        .clone()
        .ok_or_else(|| anyhow!("session mode requires session_id"))?;

    if !supports_session(settings.language) {
        writer
            .write(&Response::CellResult {
                request_id: settings.request_id,
                session_id,
                status: ResultStatus::EngineError,
                exec_time_ms: 0,
                value_repr: None,
                traceback: Some(format!(
                    "language {:?} does not support session mode",
                    settings.language
                )),
            })
            .await?;
        return Ok(());
    }

    // /var/lib/sandbox/sessions/<uuid>/ is the bind-mount source used by
    // `resolver::mount` (mounted RW as `/session`). Create it before the
    // first cell so mount::resolve picks it up.
    let session_root = PathBuf::from(SESSIONS_ROOT).join(&session_id);
    if let Err(e) = std::fs::create_dir_all(&session_root) {
        writer
            .write(&Response::CellResult {
                request_id: settings.request_id,
                session_id,
                status: ResultStatus::EngineError,
                exec_time_ms: 0,
                value_repr: None,
                traceback: Some(format!("mkdir session root: {e}")),
            })
            .await?;
        return Ok(());
    }

    // Sandbox writes cell result as nobody (uid 65534) via user-ns mapping.
    let _ = std::fs::set_permissions(&session_root, std::fs::Permissions::from_mode(0o777));

    // Clear any stale result file from an earlier session with the same id.
    let _ = std::fs::remove_file(session_root.join(CELL_RESULT_FILE));

    // Record the base settings so future ExecCell messages can inherit
    // language, runtime, limits, capabilities, network, etc.
    state
        .sessions
        .lock()
        .insert(session_id.clone(), settings.clone());

    let request_id = settings.request_id.clone();
    writer
        .write(&Response::CellResult {
            request_id,
            session_id,
            status: ResultStatus::Success,
            exec_time_ms: 0,
            value_repr: Some("session_ready".into()),
            traceback: None,
        })
        .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
#[instrument(skip(state, code, files, _stdin, writer, data), fields(req = %id, session = %session_id))]
pub async fn run_cell<W: AsyncWrite + Unpin>(
    state: &EngineState,
    id: String,
    session_id: String,
    code: String,
    files: Vec<FileEntry>,
    _stdin: Option<String>,
    wall_ms: Option<u64>,
    writer: &FrameWriter<W>,
    data: Option<&DataChannel>,
) -> Result<()> {
    let start = Instant::now();

    let base = {
        let sessions = state.sessions.lock();
        sessions.get(&session_id).cloned()
    };
    let Some(base) = base else {
        writer
            .write(&Response::CellResult {
                request_id: id,
                session_id,
                status: ResultStatus::EngineError,
                exec_time_ms: start.elapsed().as_millis() as u64,
                value_repr: None,
                traceback: Some("session not started; send Execute with mode=session first".into()),
            })
            .await?;
        return Ok(());
    };

    if !supports_session(base.language) {
        writer
            .write(&Response::CellResult {
                request_id: id,
                session_id,
                status: ResultStatus::EngineError,
                exec_time_ms: start.elapsed().as_millis() as u64,
                value_repr: None,
                traceback: Some(format!(
                    "language {:?} does not support session mode",
                    base.language
                )),
            })
            .await?;
        return Ok(());
    }

    let session_root = PathBuf::from(SESSIONS_ROOT).join(&session_id);
    // Wipe any prior result so stale data can't leak between cells.
    let _ = std::fs::remove_file(session_root.join(CELL_RESULT_FILE));

    let (shim_name, shim_body, cell_ext) = language_shim(base.language);
    let cell_filename = format!("{CELL_FILENAME}.{cell_ext}");
    // Build the per-cell settings by cloning the base and swapping the
    // entrypoint + files. Cell code goes to a stable, non-user-controlled
    // filename so the shim can locate it deterministically.
    let mut cell_settings = base.clone();
    cell_settings.request_id = id.clone();
    cell_settings.mode = protocol::Mode::Exec;
    cell_settings.entrypoint = shim_name.to_string();

    // Per-cell wall clock cap: prefer the explicit request value, fall back
    // to the session's default.
    if let Some(w) = wall_ms {
        cell_settings.limits.wall_ms = w;
    }

    // File layout: shim + cell + any per-cell user files. Session state
    // lives on the /session mount, not in /sandbox, so it survives a cell
    // completion.
    let mut file_bundle = Vec::with_capacity(2 + files.len());
    file_bundle.push(FileEntry {
        path: shim_name.to_string(),
        content: shim_body.as_bytes().to_vec(),
        mode: 0o644,
    });
    file_bundle.push(FileEntry {
        path: cell_filename.clone(),
        content: code.into_bytes(),
        mode: 0o644,
    });
    for f in files {
        // Guard against name collisions with the shim/cell files.
        if f.path == shim_name || f.path == cell_filename {
            continue;
        }
        file_bundle.push(f);
    }
    cell_settings.files = file_bundle;

    // Ensure /session gets bound. The mount resolver honours `session_id` on
    // Session mode; we're running as Exec now, so add an explicit ExtraMount
    // pointing at the session directory.
    let already_mounted = cell_settings
        .filesystem
        .mounts
        .iter()
        .any(|m| m.target == "/session");
    if !already_mounted {
        cell_settings.filesystem.mounts.push(ExtraMount {
            source: format!("session:{session_id}"),
            target: "/session".into(),
            mode: MountMode::Rw,
        });
    }

    // Session mode implies +persistent_session. Add it once so downstream
    // resolvers reason about it consistently.
    if !cell_settings
        .capabilities
        .contains(&Capability::PersistentSession)
    {
        cell_settings
            .capabilities
            .push(Capability::PersistentSession);
    }

    // Guardrail: without a launcher the sandbox cannot run at all.
    if state.launcher.is_none() {
        writer
            .write(&Response::CellResult {
                request_id: id,
                session_id,
                status: ResultStatus::EngineError,
                exec_time_ms: start.elapsed().as_millis() as u64,
                value_repr: None,
                traceback: Some("sandbox launcher unavailable on this host".into()),
            })
            .await?;
        return Ok(());
    }

    // Reuse the exec pipeline. Its Result frame will carry the RAW exit,
    // memory, and streamed output — good side effects. We only care about
    // the structured result file the shim wrote.
    if let Err(e) = run_exec_pipeline(state, cell_settings, data).await {
        writer
            .write(&Response::CellResult {
                request_id: id,
                session_id,
                status: ResultStatus::EngineError,
                exec_time_ms: start.elapsed().as_millis() as u64,
                value_repr: None,
                traceback: Some(format!("exec pipeline: {e:#}")),
            })
            .await?;
        return Ok(());
    }

    // Consume the shim's result file. If missing, the cell died before it
    // could report — surface as an EngineError with a hint.
    let result_path = session_root.join(CELL_RESULT_FILE);
    let (status, value_repr, traceback) = match std::fs::read(&result_path) {
        Ok(bytes) => parse_shim_result(&bytes),
        Err(e) => (
            ResultStatus::EngineError,
            None,
            Some(format!(
                "no cell result file at {}: {e}",
                result_path.display()
            )),
        ),
    };
    let _ = std::fs::remove_file(&result_path);

    let exec_time_ms = start.elapsed().as_millis() as u64;
    info!(status = ?status, exec_time_ms, "session_cell_done");

    writer
        .write(&Response::CellResult {
            request_id: id,
            session_id,
            status,
            exec_time_ms,
            value_repr,
            traceback,
        })
        .await?;
    Ok(())
}

async fn run_exec_pipeline(
    state: &EngineState,
    settings: Settings,
    data: Option<&DataChannel>,
) -> Result<()> {
    // We can't fully piggyback on modes::exec::run because it writes a
    // Result frame — we want the shim's structured output on a CellResult
    // frame instead. So we duplicate the tight loop but discard the frame.
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

    // Resolve, launch, and drain on ONE blocking thread — same shape as
    // exec mode; extra hops only add scheduler latency to every cell.
    let joined = {
        let launcher = launcher.clone();
        tokio::task::spawn_blocking(move || {
            let resolved = resolver_spec::resolve(&settings, work_root, Some(&binary_cache))
                .context("resolve spec")?;
            let handle = launcher.launch(&resolved.spec).context("launch")?;
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
            Ok::<_, anyhow::Error>((cg, exit))
        })
        .await
        .context("resolve/launch/drain join")??
    };

    let (cg, _child_exit) = joined;
    launcher.release_cgroup(cg);
    Ok(())
}

#[derive(Deserialize)]
struct ShimResult {
    status: String,
    #[serde(default)]
    value_repr: Option<String>,
    #[serde(default)]
    traceback: Option<String>,
}

fn parse_shim_result(bytes: &[u8]) -> (ResultStatus, Option<String>, Option<String>) {
    match serde_json::from_slice::<ShimResult>(bytes) {
        Ok(r) => {
            let status = match r.status.as_str() {
                "ok" => ResultStatus::Success,
                "error" => ResultStatus::NonZeroExit,
                _ => ResultStatus::EngineError,
            };
            (status, r.value_repr, r.traceback)
        }
        Err(e) => (
            ResultStatus::EngineError,
            None,
            Some(format!("parse cell result: {e}")),
        ),
    }
}

const fn supports_session(lang: Language) -> bool {
    matches!(lang, Language::Python | Language::Typescript)
}

fn language_shim(lang: Language) -> (&'static str, &'static str, &'static str) {
    match lang {
        Language::Python => (SHIM_FILENAME_PY, python_shim(), "py"),
        Language::Typescript => (SHIM_FILENAME_JS, typescript_shim(), "mjs"),
        _ => {
            // supports_session gates entry, so this branch is unreachable in
            // practice; return an empty shim so the caller still gets a file
            // and the resolver doesn't panic.
            warn!(?lang, "language_shim_unsupported");
            (".rockbox_session_shim.sh", "#!/bin/false\n", "txt")
        }
    }
}

/// Python session shim. Restores namespace from `/session/state.pkl`,
/// executes the cell, dumps namespace back, writes structured result to
/// `/session/.rockbox_cell_result.json`. Built once per engine process —
/// the 60-line body was being re-formatted on every cell run.
fn python_shim() -> &'static str {
    static SHIM: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SHIM.get_or_init(|| {
        format!(
            r#"import os, sys, json, pickle

STATE_PATH = "/session/state.pkl"
RESULT_PATH = "/session/{result_file}"
CELL_PATH = "/sandbox/{cell_file}.py"

globals_dict = {{"__name__": "__main__"}}
if os.path.exists(STATE_PATH):
    try:
        with open(STATE_PATH, "rb") as f:
            saved = pickle.load(f)
        # Only restore names the current interpreter can rebuild.
        for k, v in saved.items():
            globals_dict[k] = v
    except Exception as e:
        # Corrupted state — best to keep going with an empty namespace than
        # to hard-fail every subsequent cell.
        print(f"[rockbox] session state unreadable, resetting: {{e}}", file=sys.stderr)

with open(CELL_PATH, "r", encoding="utf-8") as f:
    cell_src = f.read()

result = {{"status": "ok", "value_repr": None, "traceback": None}}
try:
    code = compile(cell_src, "<cell>", "exec")
    exec(code, globals_dict)
except SystemExit as e:
    if e.code not in (None, 0):
        result = {{
            "status": "error",
            "value_repr": None,
            "traceback": f"SystemExit({{e.code}})",
        }}
except BaseException:
    import traceback  # lazy: only the error path pays for this import
    result = {{
        "status": "error",
        "value_repr": None,
        "traceback": traceback.format_exc(),
    }}

# Persist namespace. Skip modules and unpicklable values silently.
if result["status"] == "ok":
    to_save = {{}}
    for k, v in list(globals_dict.items()):
        if k.startswith("__"):
            continue
        if isinstance(v, type(sys)):
            continue  # modules
        try:
            pickle.dumps(v)
            to_save[k] = v
        except Exception:
            continue
    try:
        with open(STATE_PATH, "wb") as f:
            pickle.dump(to_save, f, protocol=pickle.HIGHEST_PROTOCOL)
    except Exception as e:
        print(f"[rockbox] failed to persist session state: {{e}}", file=sys.stderr)

with open(RESULT_PATH, "w", encoding="utf-8") as f:
    json.dump(result, f)

sys.stdout.flush()
sys.stderr.flush()
sys.exit(0 if result["status"] == "ok" else 1)
"#,
            result_file = CELL_RESULT_FILE,
            cell_file = CELL_FILENAME,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shim_dispatch_by_language() {
        let (name, body, ext) = language_shim(Language::Python);
        assert_eq!(name, SHIM_FILENAME_PY);
        assert_eq!(ext, "py");
        assert!(body.contains("pickle"));
        assert!(body.contains("/session/state.pkl"));

        let (name, body, ext) = language_shim(Language::Typescript);
        assert_eq!(name, SHIM_FILENAME_JS);
        assert_eq!(ext, "mjs");
        assert!(body.contains("vm.createContext"));
        assert!(body.contains("/session/state.json"));
    }

    #[test]
    fn shim_result_ok() {
        let payload = br#"{"status":"ok","value_repr":"42","traceback":null}"#;
        let (status, val, tb) = parse_shim_result(payload);
        assert!(matches!(status, ResultStatus::Success));
        assert_eq!(val.as_deref(), Some("42"));
        assert!(tb.is_none());
    }

    #[test]
    fn shim_result_error() {
        let payload = br#"{"status":"error","value_repr":null,"traceback":"boom"}"#;
        let (status, val, tb) = parse_shim_result(payload);
        assert!(matches!(status, ResultStatus::NonZeroExit));
        assert!(val.is_none());
        assert_eq!(tb.as_deref(), Some("boom"));
    }

    #[test]
    fn shim_result_bad_json_is_engine_error() {
        let (status, _, tb) = parse_shim_result(b"not-json");
        assert!(matches!(status, ResultStatus::EngineError));
        assert!(tb.is_some());
    }

    #[test]
    fn supports_session_matrix() {
        assert!(supports_session(Language::Python));
        assert!(supports_session(Language::Typescript));
        assert!(!supports_session(Language::Go));
        assert!(!supports_session(Language::Rust));
        assert!(!supports_session(Language::Cpp));
    }
}

/// TypeScript / Node session shim. Uses `vm.createContext` for isolation
/// and JSON for the on-disk state format (structuredClone-compatible values
/// only — functions and classes don't survive; this matches Jupyter's kernel
/// isolation semantics for JS notebooks).
fn typescript_shim() -> &'static str {
    static SHIM: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SHIM.get_or_init(|| {
        format!(
            r#"import {{ readFileSync, writeFileSync, existsSync }} from "node:fs";
import vm from "node:vm";

const STATE_PATH = "/session/state.json";
const RESULT_PATH = "/session/{result_file}";
const CELL_PATH = "/sandbox/{cell_file}.mjs";

let ctxData = {{}};
if (existsSync(STATE_PATH)) {{
  try {{
    ctxData = JSON.parse(readFileSync(STATE_PATH, "utf8"));
  }} catch (e) {{
    process.stderr.write(`[rockbox] session state unreadable, resetting: ${{e}}\n`);
  }}
}}

const ctx = vm.createContext(ctxData);
const cell = readFileSync(CELL_PATH, "utf8");

let result = {{ status: "ok", value_repr: null, traceback: null }};
try {{
  vm.runInContext(cell, ctx, {{ filename: "<cell>" }});
}} catch (e) {{
  result = {{
    status: "error",
    value_repr: null,
    traceback: (e && e.stack) ? String(e.stack) : String(e),
  }};
}}

if (result.status === "ok") {{
  const toSave = {{}};
  for (const [k, v] of Object.entries(ctx)) {{
    try {{
      JSON.stringify(v);
      toSave[k] = v;
    }} catch {{
      // Unserialisable — drop.
    }}
  }}
  try {{
    writeFileSync(STATE_PATH, JSON.stringify(toSave));
  }} catch (e) {{
    process.stderr.write(`[rockbox] failed to persist session state: ${{e}}\n`);
  }}
}}

writeFileSync(RESULT_PATH, JSON.stringify(result));
process.exit(result.status === "ok" ? 0 : 1);
"#,
            result_file = CELL_RESULT_FILE,
            cell_file = CELL_FILENAME,
        )
    })
}
