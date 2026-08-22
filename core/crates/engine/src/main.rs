//! Rockbox sandbox engine — per-VM Rust binary spawned by the Elixir
//! orchestrator. Reads msgpack [`Command`]s from stdin, drives the sandbox,
//! and writes [`Response`]s back on stdout. Raw stdout/stderr of the user's
//! code rides a separate SOCK_SEQPACKET data channel

use engine::{App, Args};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse_from_env();
    let app = App::init(args)?;
    app.run().await
}
