//! Linux implementation of the sandbox primitives.
//!
//! Each submodule implements one layer of the 10-layer stack defined in
//! `docs/nix_sandbox_flowchart.md`:
//!
//! - [`apparmor`] — `change_onexec` to per-uuid profile (SEC-09 + SEC-25)
//!
//! Most modules are stand-alone testable; integration happens in [`launcher`].

pub mod apparmor;
