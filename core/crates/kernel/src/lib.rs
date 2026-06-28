//! 10-layer sandbox primitives.
//!
//! Implementations live in [`linux`] and are gated `cfg(target_os = "linux")`.
//! On non-Linux hosts a [`stub`] module exposes the same API surface but
//! every entrypoint returns [`SandboxError::Unsupported`] so the rest of the
//! workspace compiles cleanly on dev machines (macOS, etc.).
//!
//! Public API is platform-neutral; callers code against the re-exports at the
//! crate root and the platform module switches under the hood.

pub mod error;
pub mod spec;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(not(target_os = "linux"))]
pub mod stub;
#[cfg(not(target_os = "linux"))]
pub use stub::*;

pub use error::{SandboxError, SandboxResult};
pub use spec::{ChildSpec, MountKind, ResourceLimits, SandboxLayers, SeccompProfileId};
