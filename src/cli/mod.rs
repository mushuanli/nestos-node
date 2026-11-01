//! Argument parsing and console presentation; configuration policy belongs to admin.
mod access;
mod plugins;
pub fn plugins(args: &[String]) -> Result<(), String> {
    plugins::run(args)
}

pub const HELP: &str = "cells [CONFIG]\ncells --version\ncells access --help\ncells plugins check CONFIG [PLUGIN]\ncells sync <init|verify|gc|backup|restore|repair> CONFIG [arguments]\n\nWithout a subcommand, start the configured server. access edits local configuration; restart to apply.\n\nExport metadata: the first metadata write creates <export-root>/.nestos/meta.sqlite on this node. Read-only access never creates or changes it. Subdirectory mounts share source-relative keys. Keep .nestos with the export when backing up or moving it; do not delete it as a cache.";

pub fn access(args: &[String]) -> Result<(), String> {
    if args.is_empty() || args.iter().any(|s| s == "--help" || s == "-h") {
        println!("{}", access::HELP);
        return Ok(());
    }
    let (path, command, dry_run) = access::parse(args)?;
    let result = crate::admin::access(&path, command, dry_run)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&result).map_err(|_| "Cannot encode result")?
    );
    if result.get("allowed").and_then(serde_json::Value::as_bool) == Some(false) {
        return Err("Access denied by configured directory policy".into());
    }
    Ok(())
}
