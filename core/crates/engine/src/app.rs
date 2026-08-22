//! Engine top-level lifecycle: parse args, set up the Port and data channel,
//! run the read/dispatch loop, and tear down on `shutdown` or stdin EOF

use crate::data_channel::DataChannel;
use crate::modes;
use crate::state::EngineState;
use anyhow::Result;
use msgpack::{FrameReader, FrameWriter};
use protocol::{Command, Response, ResultStatus, SCHEMA_VERSION};
use std::env;
use std::path::PathBuf;
#[cfg(not(target_os = "linux"))]
use tokio::io::{stdin, stdout};
use tracing::{error, info, instrument};

/// AsyncRead/AsyncWrite directly over an OS fd, bypassing tokio's stdio
/// blocking-thread + channel hop. The port pipes are private to this process
/// (single reader/writer), so epoll readiness is trustworthy.
#[cfg(target_os = "linux")]
mod fdio {
    use std::io;
    use std::os::fd::{AsRawFd, RawFd};
    use std::pin::Pin;
    use std::task::{Context, Poll, ready};
    use tokio::io::unix::AsyncFd;
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

    pub(crate) struct FdIo<T: AsRawFd>(AsyncFd<T>);

    impl<T: AsRawFd> FdIo<T> {
        pub(crate) fn new(inner: T) -> io::Result<Self> {
            Ok(Self(AsyncFd::new(inner)?))
        }
    }

    fn make_nonblocking(fd: RawFd) {
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags >= 0 {
                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
        }
    }

    /// The port pipes are never shared with another process, so flipping
    /// them to non-blocking for the engine's lifetime is safe.
    pub(crate) fn make_stdio_nonblocking() {
        make_nonblocking(0);
        make_nonblocking(1);
    }

    impl<T: AsRawFd> AsyncRead for FdIo<T> {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            loop {
                let mut guard = ready!(self.0.poll_read_ready(cx))?;
                match guard.try_io(|inner| {
                    let n = unsafe {
                        libc::read(
                            inner.as_raw_fd(),
                            buf.unfilled_mut().as_mut_ptr().cast::<libc::c_void>(),
                            buf.remaining(),
                        )
                    };
                    if n < 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(n as usize)
                    }
                }) {
                    Ok(Ok(0)) => return Poll::Ready(Ok(())),
                    Ok(Ok(n)) => {
                        buf.advance(n);
                        return Poll::Ready(Ok(()));
                    }
                    Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Ok(Err(e)) if e.kind() == io::ErrorKind::WouldBlock => continue,
                    Ok(Err(e)) => return Poll::Ready(Err(e)),
                    Err(_) => continue,
                }
            }
        }
    }

    impl<T: AsRawFd> AsyncWrite for FdIo<T> {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            loop {
                let mut guard = ready!(self.0.poll_write_ready(cx))?;
                match guard.try_io(|inner| {
                    let n = unsafe {
                        libc::write(
                            inner.as_raw_fd(),
                            buf.as_ptr().cast::<libc::c_void>(),
                            buf.len(),
                        )
                    };
                    if n < 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(n as usize)
                    }
                }) {
                    Ok(Ok(n)) => return Poll::Ready(Ok(n)),
                    Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Ok(Err(e)) if e.kind() == io::ErrorKind::WouldBlock => continue,
                    Ok(Err(e)) => return Poll::Ready(Err(e)),
                    Err(_) => continue,
                }
            }
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
}

#[cfg(target_os = "linux")]
fn port_io() -> Result<(
    FrameReader<fdio::FdIo<std::io::Stdin>>,
    FrameWriter<fdio::FdIo<std::io::Stdout>>,
)> {
    fdio::make_stdio_nonblocking();
    Ok((
        FrameReader::new(fdio::FdIo::new(std::io::stdin())?),
        FrameWriter::new(fdio::FdIo::new(std::io::stdout())?),
    ))
}

#[cfg(not(target_os = "linux"))]
fn port_io() -> Result<(
    FrameReader<tokio::io::Stdin>,
    FrameWriter<tokio::io::Stdout>,
)> {
    Ok((FrameReader::new(stdin()), FrameWriter::new(stdout())))
}

#[derive(Debug)]
pub struct Args {
    /// Optional path to a SOCK_SEQPACKET socket for the data channel.
    /// Created and listened on by Elixir before the engine starts.
    pub data_socket: Option<PathBuf>,

    /// Log level filter (overrides env `RUST_LOG`).
    pub log: String,
}

impl Args {
    /// Hand-rolled parsing: `engine [--data-socket <path>] [--log <level>]`.
    /// Avoids pulling clap (and its build/runtime weight) into the engine binary.
    pub fn parse_from_env() -> Self {
        let mut data_socket = env::var("ROCKBOX_DATA_SOCKET").ok().map(PathBuf::from);
        let mut log = env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());
        let mut iter = env::args();
        iter.next();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--data-socket" => data_socket = iter.next().map(PathBuf::from),
                "--log" => log = iter.next().unwrap_or(log),
                _ => {}
            }
        }
        Self { data_socket, log }
    }
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
        let (mut reader, writer) = port_io()?;

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
