//! Linux implementation of the sandbox primitives.
//!
//! Each submodule implements one layer of the 10-layer stack defined in
//! `docs/nix_sandbox_flowchart.md`:
//!
//! - [`clone3`] — fused clone + pidfd + cgroup placement (PERF-06 + SEC-17)
//! - [`apparmor`] — `change_onexec` to per-uuid profile (SEC-09 + SEC-25)
//! - [`cgroup`] — v2 + `cgroup.kill` (PERF-10)
//! - [`drain`] — io_uring stdout/stderr/pidfd/timer (PERF-08)
//! - [`pidfd`] — pidfd_open helpers for callers that don't use clone3-PIDFD
//!
//! Most modules are stand-alone testable; integration happens in [`launcher`].

pub mod apparmor;
pub mod cgroup;
pub mod clone3;
pub mod drain;
pub mod pidfd;

pub use cgroup::{Cgroup, CgroupConfig};
pub use drain::Drainer;
