//! `cells` — a single intelligent node of NestOS, integrating information and
//! agents like a cell in a honeycomb.
//!
//! # Layout
//!
//! The crate is organised so that each layer answers one question, and policy
//! is separated from mechanism throughout:
//!
//! | module | question it answers |
//! |---|---|
//! | [`core`] | what are the shared primitives (errors, ids, gates, workers)? |
//! | [`config`] | what did the operator ask for, and is it valid? |
//! | [`app`] | how is that wiredup into runtime state? |
//! | [`auth`] | who is calling, and what may they touch? |
//! | [`fs`] | what may a path be, and how is a directory operated on? |
//! | [`operations`] | has this write already been applied? |
//! | [`process`] | may a command run, and how is it sandboxed and reaped? |
//! | [`workspace`] | durable lease fencing (policy + journal mechanism) |
//! | [`http`] | how is all of that exposed over HTTP? |
//!
//! Dependencies point downwards only: `http` depends on the domain modules,
//! never the reverse, and [`core`] depends on nothing but the standard library
//! and foundational crates.

pub mod admin;
pub mod app;
pub mod cli;
pub mod config;
pub mod core;
pub mod http;
pub use plugin::common::workspace;

pub use http::router;

pub mod plugin;
pub use core::auth;
pub use plugin::common as plugins;
pub use plugin::files::{fs, operations};
pub use plugin::{harness, process, projects, sync};
