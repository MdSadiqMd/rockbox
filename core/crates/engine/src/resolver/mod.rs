//! Settings → [`ChildSpec`] mapping. Every layer of the sandbox stack reads
//! exactly one resolver's output, so adding a new capability is a localised
//! change.
//!
//! Module map:
//!
//! - [`env`] — merge runtime env + user env, apply SEC-18 strip
//! - [`limits`] — translate `Limits` → `ResourceLimits` (handles large_fs cap)
//! - [`seccomp`] — choose profile id from language + capabilities

pub mod env;
pub mod limits;
pub mod seccomp;
