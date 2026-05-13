//! 4-byte big-endian length-prefixed msgpack framing for the Elixir Port.
//!
//! Matches `Port.open(..., [:binary, {:packet, 4}, ...])` on the BEAM side.
//! - Hot path: msgpack decode via [`rmp_serde`].
//! - Size guard: rejects frames larger than [`protocol::MAX_FRAME_BYTES`].

#![forbid(unsafe_code)]

use bytes::{BufMut, BytesMut};
use protocol::{MAX_FRAME_BYTES, ProtocolError, Result};
use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;
use tracing::trace;

/// Reads frames from an async source. Owns the underlying reader and an
/// internal scratch buffer reused across reads.
#[derive(Debug)]
pub struct FrameReader<R> {
    reader: R,
    buf: BytesMut,
}

impl<R> FrameReader<R>
where
    R: AsyncRead + Unpin,
{
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            buf: BytesMut::new(),
        }
    }

    /// Read one frame and decode it as `T`. Blocks until either a full frame
    /// arrives or the stream errors / closes.
    pub async fn read<T: DeserializeOwned>(&mut self) -> Result<T> {
        let mut len_buf = [0u8; 4];
        self.reader.read_exact(&mut len_buf).await.map_err(io_err)?;
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > MAX_FRAME_BYTES {
            return Err(ProtocolError::FrameTooLarge {
                got: len,
                limit: MAX_FRAME_BYTES,
            });
        }
        self.buf.clear();
        self.buf.resize(len, 0);
        self.reader
            .read_exact(&mut self.buf[..])
            .await
            .map_err(io_err)?;
        trace!(bytes = len, "frame_in");
        let value = rmp_serde::from_slice::<T>(&self.buf)?;
        Ok(value)
    }
}

/// Async-safe writer. Multiple tasks may push frames concurrently; the mutex
/// guarantees the 4-byte prefix and payload are never interleaved.
#[derive(Debug)]
pub struct FrameWriter<W> {
    inner: Mutex<W>,
}

impl<W> FrameWriter<W>
where
    W: AsyncWrite + Unpin,
{
    pub fn new(writer: W) -> Self {
        Self {
            inner: Mutex::new(writer),
        }
    }

    pub async fn write<T: Serialize>(&self, value: &T) -> Result<()> {
        let payload = rmp_serde::to_vec_named(value)?;
        if payload.len() > MAX_FRAME_BYTES {
            return Err(ProtocolError::FrameTooLarge {
                got: payload.len(),
                limit: MAX_FRAME_BYTES,
            });
        }
        let mut scratch = BytesMut::with_capacity(4 + payload.len());
        scratch.put_u32(payload.len() as u32);
        scratch.put_slice(&payload);

        let mut w = self.inner.lock().await;
        w.write_all(&scratch).await.map_err(io_err)?;
        w.flush().await.map_err(io_err)?;
        trace!(bytes = scratch.len(), "frame_out");
        Ok(())
    }
}

fn io_err(e: std::io::Error) -> ProtocolError {
    // I/O on the port channel is fatal; the engine loop terminates and the
    // supervisor decides whether to respawn.
    ProtocolError::Decode(rmp_serde::decode::Error::Uncategorized(e.to_string()))
}
