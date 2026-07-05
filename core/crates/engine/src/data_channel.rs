//! High-rate stdout/stderr streaming channel (FIX ARCH-04 + PERF-12).
//!
//! Transport: `UnixStream` with a tiny framing header per chunk:
//! ```text
//!   1 byte  stream tag (0x01 stdout, 0x02 stderr)
//!   4 bytes BE length
//!   N bytes payload
//! ```
//!
//! The orchestrator's GenServer reads framed chunks and broadcasts them on
//! PubSub. Writer batches at the Drainer level (4ms / 4KB) so each send is
//! at least one full kernel pipe-buffer worth — keeps syscall rate sane.

use anyhow::Result;
use bytes::{BufMut, BytesMut};
use std::path::Path;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::Mutex;

pub struct DataChannel {
    inner: Mutex<UnixStream>,
}

impl std::fmt::Debug for DataChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataChannel").finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Stream {
    Stdout = 1,
    Stderr = 2,
}

impl DataChannel {
    pub async fn connect(path: &Path) -> Result<Self> {
        let sock = UnixStream::connect(path).await?;
        Ok(Self {
            inner: Mutex::new(sock),
        })
    }

    pub async fn send(&self, stream: Stream, payload: &[u8]) -> Result<()> {
        let mut frame = BytesMut::with_capacity(5 + payload.len());
        frame.put_u8(stream as u8);
        frame.put_u32(payload.len() as u32);
        frame.put_slice(payload);
        let mut guard = self.inner.lock().await;
        guard.write_all(&frame).await?;
        guard.flush().await?;
        Ok(())
    }
}
