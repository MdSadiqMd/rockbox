//! LSP relay — forwards a request to a per-language language server hosted
//! by the engine, returns the response bytes.
//!
//! Design:
//!
//! - One long-lived server child per language, held in
//!   [`EngineState::lsp_servers`]. First relay per language spawns via
//!   [`resolve_server`]; subsequent relays reuse the running child.
//! - JSON-RPC 2.0 over stdio with LSP's `Content-Length` framing.
//! - Request/response IDs are rewritten to unique per-server ids so
//!   multiple concurrent relays cannot collide.
//! - Params are decoded from msgpack → JSON on the way in, encoded back to
//!   msgpack on the way out (Elixir's Msgpax speaks msgpack natively).

use crate::state::EngineState;
use anyhow::{Context, Result, anyhow};
use msgpack::FrameWriter;
use parking_lot::Mutex;
use protocol::{Language, LspParams, Response};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::Stdout;
use tracing::{info, warn};

/// A running language server. `stdin` is protected by a mutex — we can only
/// have one relay writing at a time — and the reader lives on a background
/// thread that funnels replies into `pending` keyed by JSON-RPC id.
#[derive(Debug)]
pub struct LspServer {
    pub language: Language,
    pub bin: String,
    stdin: Mutex<Option<ChildStdin>>,
    pending: Arc<Mutex<std::collections::HashMap<i64, tokio::sync::oneshot::Sender<Value>>>>,
    next_id: AtomicI64,
    _child: Mutex<Option<Child>>,
}

impl LspServer {
    fn spawn(language: Language) -> Result<Arc<Self>> {
        let (bin, args) = resolve_server(language)?;
        let mut child = Command::new(&bin)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawn {bin}"))?;

        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| anyhow!("no stderr"))?;

        let pending: Arc<Mutex<std::collections::HashMap<i64, _>>> =
            Arc::new(Mutex::new(std::collections::HashMap::new()));

        let pending_reader = pending.clone();
        std::thread::spawn(move || reader_loop(stdout, pending_reader));

        // Drain stderr into tracing so a chatty server doesn't fill its pipe.
        std::thread::spawn(move || drain_stderr(stderr, language));

        info!(?language, %bin, "lsp_server_spawned");
        let server = Arc::new(Self {
            language,
            bin: bin.clone(),
            stdin: Mutex::new(Some(stdin)),
            pending,
            next_id: AtomicI64::new(1),
            _child: Mutex::new(Some(child)),
        });
        // Send an `initialize` immediately so the server is ready by the time
        // real relays arrive. Non-fatal on error — we just log and let the
        // first relay retry.
        if let Err(e) = server.initialize() {
            warn!(?language, error = %e, "lsp_initialize_failed");
        }
        Ok(server)
    }

    fn initialize(&self) -> Result<()> {
        let req = json!({
            "jsonrpc": "2.0",
            "id": self.alloc_id(),
            "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "rootUri": null,
                "capabilities": {},
            }
        });
        self.write_frame(&req)?;
        // Fire-and-forget: we don't block boot on the reply, the reader
        // thread will discard it since no oneshot is registered.
        Ok(())
    }

    fn alloc_id(&self) -> i64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn write_frame(&self, value: &Value) -> Result<()> {
        let body = serde_json::to_vec(value)?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        let mut guard = self.stdin.lock();
        let stdin = guard
            .as_mut()
            .ok_or_else(|| anyhow!("lsp server stdin closed"))?;
        stdin.write_all(header.as_bytes())?;
        stdin.write_all(&body)?;
        stdin.flush()?;
        Ok(())
    }

    async fn call(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.alloc_id();
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let (tx, rx) = tokio::sync::oneshot::channel::<Value>();
        self.pending.lock().insert(id, tx);
        self.write_frame(&req)?;

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(_)) => {
                self.pending.lock().remove(&id);
                Err(anyhow!("lsp reader closed"))
            }
            Err(_) => {
                self.pending.lock().remove(&id);
                Err(anyhow!(
                    "lsp call timed out after {}ms",
                    timeout.as_millis()
                ))
            }
        }
    }
}

fn reader_loop(
    stdout: ChildStdout,
    pending: Arc<Mutex<std::collections::HashMap<i64, tokio::sync::oneshot::Sender<Value>>>>,
) {
    let mut reader = BufReader::new(stdout);
    loop {
        let content_length = match read_content_length(&mut reader) {
            Ok(Some(n)) => n,
            Ok(None) => break, // clean EOF
            Err(e) => {
                warn!(error = %e, "lsp_read_header_failed");
                break;
            }
        };
        let mut buf = vec![0u8; content_length];
        if let Err(e) = reader.read_exact(&mut buf) {
            warn!(error = %e, "lsp_read_body_failed");
            break;
        }
        let val: Value = match serde_json::from_slice(&buf) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "lsp_json_parse_failed");
                continue;
            }
        };
        if let Some(id) = val.get("id").and_then(Value::as_i64) {
            if let Some(tx) = pending.lock().remove(&id) {
                let _ = tx.send(val);
            }
        }
        // Notifications and orphan responses are dropped intentionally —
        // we only surface direct replies to callers.
    }
}

fn read_content_length<R: BufRead>(r: &mut R) -> Result<Option<usize>> {
    let mut len: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = r.read_line(&mut line)?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(&['\r', '\n'][..]);
        if trimmed.is_empty() {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            len = rest.trim().parse::<usize>().ok();
        }
    }
    Ok(len.map(|n| n))
}

fn drain_stderr(stderr: std::process::ChildStderr, language: Language) {
    let mut r = BufReader::new(stderr);
    let mut line = String::new();
    loop {
        line.clear();
        match r.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let msg = line.trim_end_matches(&['\r', '\n'][..]);
                if !msg.is_empty() {
                    warn!(?language, msg, "lsp_stderr");
                }
            }
        }
    }
}

/// Map a Language to its language-server binary + argv. Servers must speak
/// LSP over stdio. If the binary is missing on `$PATH`, spawning fails and
/// the relay returns a structured error to the caller.
fn resolve_server(language: Language) -> Result<(String, Vec<String>)> {
    let (bin, args): (&str, &[&str]) = match language {
        Language::Python => ("pyright-langserver", &["--stdio"]),
        Language::Typescript => ("typescript-language-server", &["--stdio"]),
        Language::Go => ("gopls", &["-mode=stdio"]),
        Language::Rust => ("rust-analyzer", &[]),
        Language::Cpp => ("clangd", &[]),
    };
    Ok((
        bin.to_string(),
        args.iter().map(|s| s.to_string()).collect(),
    ))
}

pub async fn relay(
    state: &EngineState,
    params: LspParams,
    writer: &FrameWriter<Stdout>,
) -> Result<()> {
    let start = Instant::now();
    let Some(language) = state.language() else {
        writer
            .write(&Response::LspResponse {
                req_id: params.req_id,
                result: Vec::new(),
                error: Some("no active language; send Execute with a language before Lsp".into()),
            })
            .await?;
        return Ok(());
    };

    let server = get_or_spawn(state, language).await?;

    // Decode the msgpack params → JSON.
    let params_json: Value = if params.params.is_empty() {
        Value::Null
    } else {
        rmp_serde::from_slice(&params.params)
            .map_err(|e| anyhow!("decode lsp params msgpack: {e}"))?
    };

    // Cap per-call so a slow server can't tie up the engine forever. The
    // client can retry after retrieving control.
    let timeout = Duration::from_secs(10);

    let response = match server.call(&params.method, params_json, timeout).await {
        Ok(v) => v,
        Err(e) => {
            writer
                .write(&Response::LspResponse {
                    req_id: params.req_id,
                    result: Vec::new(),
                    error: Some(e.to_string()),
                })
                .await?;
            return Ok(());
        }
    };

    if let Some(err) = response.get("error") {
        writer
            .write(&Response::LspResponse {
                req_id: params.req_id,
                result: Vec::new(),
                error: Some(err.to_string()),
            })
            .await?;
        return Ok(());
    }

    let result = response.get("result").cloned().unwrap_or(Value::Null);
    let payload =
        rmp_serde::to_vec_named(&result).map_err(|e| anyhow!("encode lsp result msgpack: {e}"))?;

    info!(
        req_id = params.req_id,
        method = %params.method,
        elapsed_ms = start.elapsed().as_millis() as u64,
        "lsp_relay_done"
    );

    writer
        .write(&Response::LspResponse {
            req_id: params.req_id,
            result: payload,
            error: None,
        })
        .await?;
    Ok(())
}

async fn get_or_spawn(state: &EngineState, language: Language) -> Result<Arc<LspServer>> {
    if let Some(s) = state.lsp_servers.lock().get(&language).cloned() {
        return Ok(s);
    }
    let server = LspServer::spawn(language)?;
    state
        .lsp_servers
        .lock()
        .entry(language)
        .or_insert_with(|| server.clone());
    Ok(state
        .lsp_servers
        .lock()
        .get(&language)
        .cloned()
        .expect("just inserted"))
}
