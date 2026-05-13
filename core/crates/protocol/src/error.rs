use thiserror::Error;

pub type Result<T> = core::result::Result<T, ProtocolError>;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ProtocolError {
    #[error("frame larger than {limit} bytes (got {got})")]
    FrameTooLarge { got: usize, limit: usize },

    #[error("unsupported schema version {version} (engine speaks {expected})")]
    UnsupportedSchema {
        version: String,
        expected: &'static str,
    },

    #[error("msgpack decode: {0}")]
    Decode(#[from] rmp_serde::decode::Error),

    #[error("msgpack encode: {0}")]
    Encode(#[from] rmp_serde::encode::Error),

    #[error("invalid request id: {0}")]
    InvalidRequestId(String),

    #[error("settings invariant violated: {0}")]
    InvariantViolation(String),
}
