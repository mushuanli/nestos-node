//! Explicit legacy migration preserves the owner's credential, identity and effective grants.
use super::edit::tables;
use crate::config::{inspect, model::Access, Config};
use toml_edit::{value, DocumentMut, Item, Table};

pub(super) fn apply(doc: &mut DocumentMut) -> Result<(), String> {
    let config: Config =
        toml::from_str(&doc.to_string()).map_err(|_| "Invalid configuration schema")?;
    inspect::policy(&config)?;
    if !config.principals.is_empty() {
        return Ok(());
    }
    if config
        .harnesses
        .iter()
        .any(|h| !h.projects || !h.workspaces.is_empty())
    {
        return Err(
            "Migrate legacy harness workspaces to project-scoped profiles before access init"
                .into(),
        );
    }
    let owner = owner(doc, &config)?;
    if config.server_id.is_none() {
        doc["server_id"] = value(server_id()?);
    }
    tables(doc.as_table_mut(), "principals")?.push(owner);
    owner_grants(doc, &config)?;
    for row in tables(doc.as_table_mut(), "harnesses")?.iter_mut() {
        row["principal_id"] = value("owner");
    }
    Ok(())
}
fn owner(doc: &mut DocumentMut, config: &Config) -> Result<Table, String> {
    let mut owner = Table::new();
    owner["id"] = value("owner");
    for (from, to) in [
        ("api_key", "token"),
        ("api_key_env", "token_env"),
        ("token", "token"),
        ("token_env", "token_env"),
        ("password", "password"),
        ("password_env", "password_env"),
        ("username", "username"),
    ] {
        if let Some(item) = doc.as_table_mut().remove(from) {
            owner.insert(to, item);
        }
    }
    if (config.password.is_some() || config.password_env.is_some()) && config.username.is_none() {
        owner["username"] =
            value(std::env::var("FS_SERVER_USER").map_err(|_| "Missing legacy username")?);
    }
    Ok(owner)
}
fn owner_grants(doc: &mut DocumentMut, config: &Config) -> Result<(), String> {
    for (index, row) in tables(doc.as_table_mut(), "exports")?
        .iter_mut()
        .enumerate()
    {
        let grants = tables(row, "grants")?;
        let existing = grants
            .iter()
            .position(|g| g.get("principal").and_then(Item::as_str) == Some("owner"));
        if let Some(i) = existing {
            grants.remove(i);
        }
        let mut grant = Table::new();
        grant["principal"] = value("owner");
        grant["access"] = value(if config.exports[index].access == Access::Rw {
            "rw"
        } else {
            "ro"
        });
        grant["execute"] = value(config.execution);
        grants.push(grant);
    }
    Ok(())
}

fn server_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|_| "Random source unavailable")?;
    Ok(format!(
        "cells-{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ))
}
