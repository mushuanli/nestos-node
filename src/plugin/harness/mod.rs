//! Host harness profiles, separate from short-lived sandboxed shell commands.
mod attachments;
mod bridge;
#[path = "drivers/claude/mod.rs"]
mod claude;
#[path = "drivers/codex.rs"]
mod codex;
#[path = "drivers/codex/delete.rs"]
mod codex_delete;
mod command_outputs;
mod command_runs;
mod commands;
pub mod config;
mod diagnostics;
mod digest;
mod driver;
mod driver_slot;
#[path = "drivers/dsh/mod.rs"]
mod dsh;
mod environment;
mod events;
mod history;
mod models;
mod metrics;
mod plugins;
mod presentation;
mod receipts;
mod search;
mod service;
mod subscription;
mod usage;
mod writer_lock;
pub use driver::{HarnessDriver, NativeExecution};
pub use plugins::{HarnessPlugin, HarnessPlugins};
pub use service::Harnesses;
mod mutation_queue;

pub(crate) mod plugin;

mod contribution;

pub use plugin::Harness as Plugin;
