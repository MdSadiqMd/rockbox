//! Mode dispatchers. The engine picks one based on `Settings.mode`:
//!
//! - [`rl`]        — RL stepper (RL-01..08)
//! - [`lsp`]       — pass-through to language server hosted inside the sandbox

pub mod lsp;
pub mod session;
