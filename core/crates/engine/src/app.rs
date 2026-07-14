//! Engine top-level lifecycle: parse args, set up the Port and data channel,
//! run the read/dispatch loop, and tear down on `shutdown` or stdin EOF

use crate::data_channel::DataChannel;
use crate::modes;
use crate::state::EngineState;
use anyhow::Result;
use clap::Parser;
use msgpack::{FrameReader, FrameWriter};
use protocol::{Command, Response, ResultStatus, SCHEMA_VERSION};
use std::path::PathBuf;
use tokio::io::{stdin, stdout};
use tracing::{error, info, instrument};

#[derive(Debug, Parser)]
#[command(name = "engine", version, about = "Rockbox per-VM Rust engine.")]
pub struct Args {
    /// Optional path to a SOCK_SEQPACKET socket for the data channel.
    /// Created and listened on by Elixir before the engine starts.
    #[arg(long, env = "ROCKBOX_DATA_SOCKET")]
    pub data_socket: Option<PathBuf>,

    /// Log level filter (overrides env `RUST_LOG`).
    #[arg(long, env = "RUST_LOG", default_value = "info")]
    pub log: String,
}

#[derive(Debug)]
pub struct App {
    pub args: Args,
    pub state: EngineState,
}

impl App {
    pub fn init(args: Args) -> Result<Self> {
        // Logs go to stderr (stdout is the Port control channel).
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(&args.log))
            .with_writer(std::io::stderr)
            .json()
            .init();
        info!(schema = SCHEMA_VERSION, "engine_boot");
        let state = EngineState::new();
        Ok(Self { args, state })
    }

    #[instrument(skip(self))]
    pub async fn run(self) -> Result<()> {
        let Self { args, state } = self;
        let mut reader = FrameReader::new(stdin());
        let writer = FrameWriter::new(stdout());

        // Data channel is best-effort: if the orchestrator hasn't bound the
        // listener (or doesn't support it), stdout/stderr flow back through
        // the Result frame on the control channel instead.
        let data = match args.data_socket {
            Some(p) => match DataChannel::connect(&p).await {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::warn!(error = %e, path = %p.display(), "data_channel_unavailable");
                    None
                }
            },
            None => None,
        };

        // Send the boot-ready signal so the orchestrator can stop blocking.
        writer
            .write(&Response::Ready {
                language: protocol::Language::Python, // overwritten on first Execute
                runtime: "<not-yet-resolved>".into(),
                env_cached: false,
                pid: std::process::id(),
            })
            .await?;

        loop {
            let cmd: Command = match reader.read().await {
                Ok(c) => c,
                Err(e) => {
                    error!(?e, "port_read_failed");
                    break;
                }
            };

            let outcome = match cmd {
                Command::Execute(settings) => {
                    modes::dispatch(&state, *settings, &writer, data.as_ref()).await
                }
                Command::ExecCell {
                    id,
                    session_id,
                    code,
                    files,
                    stdin: input,
                    wall_ms,
                } => {
                    modes::session::run_cell(
                        &state,
                        id,
                        session_id,
                        code,
                        files,
                        input,
                        wall_ms,
                        &writer,
                        data.as_ref(),
                    )
                    .await
                }
                Command::RlStep {
                    id,
                    episode_id,
                    action,
                } => modes::rl::step(&state, id, episode_id, action, &writer, data.as_ref()).await,
                Command::Stdin { data: bytes } => state.send_stdin(&bytes).await,
                Command::Interrupt { id } => state.interrupt(&id).await,
                Command::Lsp(p) => modes::lsp::relay(&state, p, &writer).await,
                Command::Shutdown => {
                    info!("shutdown_requested");
                    break;
                }
            };
            if let Err(e) = outcome {
                error!(?e, "command_failed");
                let _ = writer
                    .write(&Response::Result {
                        request_id: "unknown".into(),
                        status: ResultStatus::EngineError,
                        exit_code: -1,
                        exec_time_ms: 0,
                        memory_peak_mb: 0,
                        cpu_time_ms: 0,
                        output_bytes: 0,
                        output_truncated: false,
                        output: String::new(),
                        errors: format!("{e:#}"),
                    })
                    .await;
            }
        }

        info!("engine_exit");
        Ok(())
    }
}
