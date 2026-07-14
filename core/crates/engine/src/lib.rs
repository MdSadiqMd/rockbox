//! Engine library surface — exposed as a library so integration tests can
//! drive the same code path the binary uses. See [`App::run`].

#![allow(clippy::missing_const_for_fn)]

pub mod app;
pub mod data_channel;
pub mod modes;
pub mod resolver;
pub mod runtime_catalog;
pub mod state;

pub use app::{App, Args};
