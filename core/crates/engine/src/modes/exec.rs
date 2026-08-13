//! `mode=exec` — single-shot sandboxed run
use crate::data_channel::{DataChannel, Stream};
use crate::modes::work_root;
use crate::resolver::spec as resolver_spec;
use crate::state::EngineState;
use anyhow::{Context, Result};
use msgpack::FrameWriter;
use protocol::{Response, ResultStatus, Settings};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use tokio::io::Stdout;
use tracing::{info, instrument};

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

#[instrument(skip(state, settings, writer, data), fields(req = %settings.request_id))]
pub async fn run(
    state: &EngineState,
    settings: Settings,
    writer: &FrameWriter<Stdout>,
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
    let output_bytes = settings.limits.output_bytes;
    let cap = output_bytes as usize;
    let stream_enabled = settings.output.stream;

    let start = Instant::now();

    // Resolve + launch run on a blocking thread: a cold compile or the
    // mount-namespace setup can take milliseconds, and the async worker must
    // stay free to service Stdin/Interrupt frames and the data channel.
    let handle = {
        let launcher = launcher.clone();
        let resolve = tokio::task::spawn_blocking(move || {
            let t = Instant::now();
            let resolved = resolver_spec::resolve(&settings, work_root, Some(&binary_cache))
                .context("resolve spec")?;
            let resolve_ms = t.elapsed().as_millis() as u64;
            let t = Instant::now();
            let handle = launcher.launch(&resolved.spec).context("launch")?;
            Ok::<_, anyhow::Error>((handle, resolve_ms, t.elapsed().as_millis() as u64))
        });
        resolve.await.context("resolve/launch join")??
    };
    let (handle, resolve_ms, launch_ms) = handle;
    let (cg, mut drainer) = launcher
        .make_drainer(handle, output_bytes)
        .context("make_drainer")?;
    let drain_start = Instant::now();

    // Chunks flow: blocking drainer → unbounded channel → async aggregator.
    let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::unbounded_channel::<(Stream, Vec<u8>)>();
    let output_bytes = Arc::new(AtomicU64::new(0));
    let counter = output_bytes.clone();

    let blocking = tokio::task::spawn_blocking(move || {
        let tx_out = chunk_tx.clone();
        let tx_err = chunk_tx.clone();
        let on_stdout = move |chunk: &[u8]| {
            counter.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            let _ = tx_out.send((Stream::Stdout, chunk.to_vec()));
        };
        let on_stderr = move |chunk: &[u8]| {
            let _ = tx_err.send((Stream::Stderr, chunk.to_vec()));
        };
        let res = drainer.run(&cg, on_stdout, on_stderr);
        // Dropping `chunk_tx` closes the channel so the aggregator exits.
        drop(chunk_tx);
        res.map(|exit| (cg, exit))
    });

    // Aggregator runs concurrently; consumes until the channel closes.
    // Output is captured up to the user-configured `output_bytes` cap so we
    // can always return it on the control channel — the data channel is a
    // streaming optimisation, not the source of truth.
    let mut output = ByteCap::new(cap);
    let mut errors = ByteCap::new(cap);
    while let Some((stream, bytes)) = chunk_rx.recv().await {
        match stream {
            Stream::Stdout => output.extend(&bytes),
            Stream::Stderr => errors.extend(&bytes),
        }
        if stream_enabled {
            if let Some(d) = data {
                d.send(stream, bytes);
            }
        }
    }

    let (cg_exit, child_exit) = blocking
        .await
        .context("drainer join")?
        .context("drainer run")?;
    let exec_ms = start.elapsed().as_millis() as u64;
    let drain_ms = drain_start.elapsed().as_millis() as u64;
    let mem_peak = cg_exit.current_memory_peak().unwrap_or(0) / (1024 * 1024);
    launcher.release_cgroup(cg_exit);

    let (status, exit_code) = map_status(&child_exit);
    info!(
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
            output_bytes: output_bytes.load(Ordering::Relaxed),
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
