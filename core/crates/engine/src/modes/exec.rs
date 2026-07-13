//! `mode=exec` — single-shot sandboxed run
use crate::data_channel::{DataChannel, Stream};
use crate::resolver::spec as resolver_spec;
use crate::state::EngineState;
use anyhow::{Context, Result};
use msgpack::FrameWriter;
use protocol::{Response, ResultStatus, Settings};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use tokio::io::Stdout;
use tracing::{info, instrument};

#[instrument(skip(state, settings, writer, data), fields(req = %settings.request_id))]
pub async fn run(
    state: &EngineState,
    settings: Settings,
    writer: &FrameWriter<Stdout>,
    data: Option<&DataChannel>,
) -> Result<()> {
    let request_id = settings.request_id.clone();
    let work_root: PathBuf = std::env::temp_dir().join("rockbox-work");
    std::fs::create_dir_all(&work_root)?;

    let resolved = resolver_spec::resolve(&settings, &work_root).context("resolve spec")?;
    let start = Instant::now();

    let launcher = match state.launcher.as_ref() {
        Some(l) => l.clone(),
        None => {
            super::die(writer, "platform_unsupported", Some("Linux-only".into())).await;
            return Ok(());
        }
    };

    let handle = launcher.launch(&resolved.spec).context("launch")?;
    let (cg, mut drainer) = launcher
        .make_drainer(handle, settings.limits.output_bytes)
        .context("make_drainer")?;

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
    let mut output = String::new();
    let mut errors = String::new();
    let cap = settings.limits.output_bytes as usize;
    while let Some((stream, bytes)) = chunk_rx.recv().await {
        if let Some(d) = data {
            let _ = d.send(stream, &bytes).await;
        }
        let buf = match stream {
            Stream::Stdout => &mut output,
            Stream::Stderr => &mut errors,
        };
        if buf.len() < cap {
            let room = cap - buf.len();
            let slice = if bytes.len() > room {
                &bytes[..room]
            } else {
                &bytes[..]
            };
            buf.push_str(&String::from_utf8_lossy(slice));
        }
    }

    let (cg_exit, child_exit) = blocking
        .await
        .context("drainer join")?
        .context("drainer run")?;
    let exec_ms = start.elapsed().as_millis() as u64;
    let mem_peak = cg_exit.current_memory_peak().unwrap_or(0) / (1024 * 1024);
    let _ = cg_exit.reset_peaks();

    let (status, exit_code) = map_status(&child_exit);
    info!(?child_exit, exec_ms, "exec_done");

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
            output,
            errors,
        })
        .await?;
    Ok(())
}

fn map_status(exit: &kernel::ChildExit) -> (ResultStatus, i32) {
    use kernel::ChildExit;
    match *exit {
        ChildExit::Normal { status, .. } if status == 0 => (ResultStatus::Success, 0),
        ChildExit::Normal { status, .. } => (ResultStatus::NonZeroExit, status),
        ChildExit::Signal { signo } => (ResultStatus::SeccompKill, -signo),
        ChildExit::Timeout => (ResultStatus::Timeout, -1),
        ChildExit::OomKilled => (ResultStatus::OomKilled, -1),
        ChildExit::OutputCap => (ResultStatus::OutputExceeded, -1),
    }
}
