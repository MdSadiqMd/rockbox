//! `mode=exec` — single-shot sandboxed run
use crate::data_channel::{DataChannel, Stream};
use crate::modes::work_root;
use crate::resolver::spec as resolver_spec;
use crate::state::EngineState;
use anyhow::{Context, Result};
use msgpack::FrameWriter;
use protocol::{Response, ResultStatus, Settings};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use tokio::io::AsyncWrite;
use tracing::debug;

/// Accumulate a stream's bytes with a caller-supplied cap; the single lossy
/// UTF-8 conversion happens once, after the last chunk, instead of per chunk.
struct ByteCap {
    buf: Vec<u8>,
    cap: usize,
}

impl ByteCap {
    fn new(cap: usize) -> Self {
        // Cap the preallocation: pathological programs that emit garbage
        // still never allocate more than they could legally send.
        Self {
            buf: Vec::with_capacity(cap.min(64 * 1024)),
            cap,
        }
    }

    fn extend(&mut self, bytes: &[u8]) {
        if self.buf.len() >= self.cap {
            return;
        }
        let room = self.cap - self.buf.len();
        let slice = if bytes.len() > room {
            &bytes[..room]
        } else {
            bytes
        };
        self.buf.extend_from_slice(slice);
    }

    fn into_lossy_string(self) -> String {
        String::from_utf8_lossy(&self.buf).into_owned()
    }
}

pub async fn run<W: AsyncWrite + Unpin>(
    state: &EngineState,
    settings: Settings,
    writer: &FrameWriter<W>,
    data: Option<&DataChannel>,
) -> Result<()> {
    let request_id = settings.request_id.clone();
    let work_root = work_root();

    let launcher = match state.launcher.as_ref() {
        Some(l) => l.clone(),
        None => {
            super::die(writer, "platform_unsupported", Some("Linux-only".into())).await;
            return Ok(());
        }
    };
    let binary_cache = state.binary_cache.clone();
    let output_bytes_cap = settings.limits.output_bytes;
    let cap = output_bytes_cap as usize;
    let stream_enabled = settings.output.stream;
    // Owned handle so the drain closure can stream without borrowing.
    let data_owned = data.cloned();

    let start = Instant::now();

    // Resolve, launch, and drain all run on ONE blocking thread. Every extra
    // hop (a second spawn_blocking dispatch, an mpsc channel, an aggregator
    // task wakeup) is pure scheduler latency on the request path; the
    // callbacks here are cheap (buffer append + optional queue push), so the
    // async side has nothing to do until the child exits.
    let joined = {
        let launcher = launcher.clone();
        tokio::task::spawn_blocking(move || {
            let t = Instant::now();
            let resolved = resolver_spec::resolve(&settings, work_root, Some(&binary_cache))
                .context("resolve spec")?;
            let resolve_ms = t.elapsed().as_millis() as u64;
            tracing::debug!(?resolved.spec, "spec_for_launch");
            let t = Instant::now();
            let handle = launcher.launch(&resolved.spec).context("launch")?;
            let launch_ms = t.elapsed().as_millis() as u64;
            let (cg, mut drainer) = launcher
                .make_drainer(handle, output_bytes_cap)
                .context("make_drainer")?;
            let drain_start = Instant::now();

            let mut output = ByteCap::new(cap);
            let mut errors = ByteCap::new(cap);
            let sent = AtomicU64::new(0);

            let on_stdout = |chunk: &[u8]| {
                sent.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                output.extend(chunk);
                if stream_enabled {
                    if let Some(d) = &data_owned {
                        d.send(Stream::Stdout, chunk.to_vec());
                    }
                }
            };
            let on_stderr = |chunk: &[u8]| {
                sent.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                errors.extend(chunk);
                if stream_enabled {
                    if let Some(d) = &data_owned {
                        d.send(Stream::Stderr, chunk.to_vec());
                    }
                }
            };

            let exit = drainer.run(&cg, on_stdout, on_stderr).context("drain")?;
            let drain_ms = drain_start.elapsed().as_millis() as u64;
            Ok::<_, anyhow::Error>((
                cg,
                exit,
                output,
                errors,
                sent.into_inner(),
                resolve_ms,
                launch_ms,
                drain_ms,
            ))
        })
        .await
        .context("resolve/launch/drain join")??
    };
    let (cg, child_exit, output, errors, output_total, resolve_ms, launch_ms, drain_ms) = joined;
    let exec_ms = start.elapsed().as_millis() as u64;

    // The drainer already read the cgroup peak for a Normal exit; don't
    // re-read /proc for the common path.
    let mem_peak = match &child_exit {
        kernel::ChildExit::Normal { memory_peak_mb, .. } => *memory_peak_mb,
        _ => cg.current_memory_peak().unwrap_or(0) / (1024 * 1024),
    };
    launcher.release_cgroup(cg);

    let (status, exit_code) = map_status(&child_exit);
    debug!(
        ?child_exit,
        exec_ms, resolve_ms, launch_ms, drain_ms, "exec_done"
    );

    writer
        .write(&Response::Result {
            request_id,
            status,
            exit_code,
            exec_time_ms: exec_ms,
            memory_peak_mb: mem_peak,
            cpu_time_ms: 0,
            output_bytes: output_total,
            output_truncated: matches!(status, ResultStatus::OutputExceeded),
            output: output.into_lossy_string(),
            errors: errors.into_lossy_string(),
        })
        .await?;
    Ok(())
}

fn map_status(exit: &kernel::ChildExit) -> (ResultStatus, i32) {
    use kernel::ChildExit;
    match *exit {
        ChildExit::Normal { status: 0, .. } => (ResultStatus::Success, 0),
        ChildExit::Normal { status, .. } => (ResultStatus::NonZeroExit, status),
        ChildExit::Signal { signo } => (ResultStatus::SeccompKill, -signo),
        ChildExit::Timeout => (ResultStatus::Timeout, -1),
        ChildExit::OomKilled => (ResultStatus::OomKilled, -1),
        ChildExit::OutputCap => (ResultStatus::OutputExceeded, -1),
    }
}
