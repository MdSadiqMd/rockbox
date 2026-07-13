//! Mode dispatchers. The engine picks one based on `Settings.mode`:
//!
//! - [`lsp`]       — pass-through to language server hosted inside the sandbox.

pub mod lsp;
