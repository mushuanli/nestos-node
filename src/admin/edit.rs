//! Configuration mutation policy. File transactions and CLI parsing live elsewhere.
use super::{migrate, AccessCommand, Credential};
use crate::config::{exports, model::PrincipalConfig, Config};
use toml_edit::{value, Array, ArrayOfTables, DocumentMut, Item, Table};

pub(super) fn apply(doc: &mut DocumentMut, command: AccessCommand) -> Result<(), String> {
    if matches!(command, AccessCommand::Init) {
        migrate::apply(doc)?;
        return inline_export_grants(doc);
    }
    let config: Config =
        toml::from_str(&doc.to_string()).map_err(|_| "Invalid configuration schema")?;
    if config.principals.is_empty() {
        return Err("Run access init before editing per-principal directory policy".into());
    }
    dispatch(doc, &config, command)?;
    inline_export_grants(doc)
}
fn inline_export_grants(doc: &mut DocumentMut) -> Result<(), String> {
    for export in tables(doc.as_table_mut(), "exports")?.iter_mut() {
        inline_grants(export);
    }
    Ok(())
}
fn inline_grants(export: &mut Table) {
    if let Some(grants) = export.get("grants").and_then(Item::as_array_of_tables) {
        let mut array = Array::new();
        let mut comments = String::new();
        for grant in grants {
            comments.push_str(&grant_comments(grant));
            array.push(grant.clone().into_inline_table());
        }
        export["grants"] = value(array);
        if !comments.is_empty() {
            export
                .key_mut("grants")
                .unwrap()
                .leaf_decor_mut()
                .set_prefix(comments);
        }
    }
    if let Some(mut key) = export.key_mut("grants") {
        key.leaf_decor_mut().set_suffix(" ");
    }
}
fn grant_comments(grant: &Table) -> String {
    let mut result = String::new();
    let mut retain = |raw: Option<&toml_edit::RawString>| {
        for line in raw.and_then(|value| value.as_str()).unwrap_or("").lines() {
            if line.trim_start().starts_with('#') {
                result.push_str(line.trim_start());
                result.push('\n');
            }
        }
    };
    retain(grant.decor().prefix());
    for (key, item) in grant.iter() {
        if let Some(key) = grant.key(key) {
            retain(key.leaf_decor().prefix());
        }
        if let Some(value) = item.as_value() {
            retain(value.decor().suffix());
        }
    }
    result
}
fn dispatch(doc: &mut DocumentMut, config: &Config, command: AccessCommand) -> Result<(), String> {
    use AccessCommand::*;
    match command {
        AddPrincipal { id, credential } => add_principal(doc, &id, credential),
        SetCredential { id, credential } => set_credential(doc, &id, credential),
        RemovePrincipal { id } => remove_principal(doc, config, &id),
        AddShare {
            alias,
            path,
            access,
            public,
        } => add_share(doc, &alias, &path, &access, public),
        SetShare {
            alias,
            access,
            public,
        } => set_share(doc, config, &alias, access, public),
        RemoveShare { alias } => {
            let i = share_index(config, &alias)?;
            tables(doc.as_table_mut(), "exports")?.remove(i);
            Ok(())
        }
        Grant {
            principal,
            alias,
            access,
            execute,
        } => grant(doc, config, &principal, &alias, &access, execute),
        Revoke { principal, alias } => revoke(doc, config, &principal, &alias),
        _ => Err("Expected a configuration edit".into()),
    }
}
pub(super) fn tables<'a>(table: &'a mut Table, key: &str) -> Result<&'a mut ArrayOfTables, String> {
    if table.get(key).is_none() {
        table.insert(key, Item::ArrayOfTables(ArrayOfTables::new()));
    }
    if let Some(array) = table.get(key).and_then(Item::as_array) {
        let mut normalized = ArrayOfTables::new();
        for item in array {
            normalized.push(
                item.as_inline_table()
                    .ok_or("Expected configuration table entries")?
                    .clone()
                    .into_table(),
            );
        }
        table.insert(key, Item::ArrayOfTables(normalized));
    }
    table
        .get_mut(key)
        .and_then(Item::as_array_of_tables_mut)
        .ok_or_else(|| "Expected configuration table entries".into())
}
fn add_principal(doc: &mut DocumentMut, id: &str, credential: Credential) -> Result<(), String> {
    let rows = tables(doc.as_table_mut(), "principals")?;
    if rows
        .iter()
        .any(|t| t.get("id").and_then(Item::as_str) == Some(id))
    {
        return Err("Principal already exists; use principal credential to rotate it".into());
    }
    let mut entry = Table::new();
    entry["id"] = value(id);
    write_credential(&mut entry, credential)?;
    rows.push(entry);
    Ok(())
}
fn set_credential(doc: &mut DocumentMut, id: &str, credential: Credential) -> Result<(), String> {
    let row = tables(doc.as_table_mut(), "principals")?
        .iter_mut()
        .find(|t| t.get("id").and_then(Item::as_str) == Some(id))
        .ok_or("Unknown principal")?;
    write_credential(row, credential)
}
fn write_credential(row: &mut Table, credential: Credential) -> Result<(), String> {
    for key in ["token", "token_env", "username", "password", "password_env"] {
        row.remove(key);
    }
    let environment = match &credential {
        Credential::Bearer { environment } | Credential::Basic { environment, .. } => environment,
    };
    if environment.is_empty()
        || !environment
            .bytes()
            .enumerate()
            .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || i > 0 && b.is_ascii_digit())
    {
        return Err("Invalid credential environment variable name".into());
    }
    match credential {
        Credential::Bearer { environment } => {
            row["token_env"] = value(environment);
        }
        Credential::Basic {
            username,
            environment,
        } => {
            row["username"] = value(username);
            row["password_env"] = value(environment);
        }
    }
    Ok(())
}
fn principal<'a>(config: &'a Config, id: &str) -> Result<&'a PrincipalConfig, String> {
    config
        .principals
        .iter()
        .find(|p| p.id == id)
        .ok_or_else(|| "Unknown principal".into())
}
fn remove_principal(doc: &mut DocumentMut, config: &Config, id: &str) -> Result<(), String> {
    principal(config, id)?;
    if config.principals.len() == 1 {
        return Err("Cannot remove the last principal".into());
    }
    if config
        .exports
        .iter()
        .any(|e| e.grants.iter().any(|g| g.principal == id))
        || config
            .harnesses
            .iter()
            .any(|h| h.principal_id.as_deref() == Some(id))
        || config
            .sync
            .as_ref()
            .is_some_and(|s| s.enabled && s.principal_id == id)
    {
        return Err("Principal is referenced by directory grants, harnesses or sync; remove its references first".into());
    }
    let rows = tables(doc.as_table_mut(), "principals")?;
    let index = rows
        .iter()
        .position(|t| t.get("id").and_then(Item::as_str) == Some(id))
        .ok_or("Unknown principal")?;
    rows.remove(index);
    Ok(())
}
fn add_share(
    doc: &mut DocumentMut,
    alias: &str,
    path: &str,
    access: &str,
    public: bool,
) -> Result<(), String> {
    if !std::path::Path::new(path).is_absolute() {
        return Err("Shared directory path must be absolute".into());
    }
    access_value(access)?;
    let mut row = Table::new();
    row["alias"] = value(alias);
    row["path"] = value(path);
    row["access"] = value(access);
    row["public"] = value(public);
    tables(doc.as_table_mut(), "exports")?.push(row);
    Ok(())
}
fn share_index(config: &Config, alias: &str) -> Result<usize, String> {
    config
        .exports
        .iter()
        .enumerate()
        .find_map(|(i, e)| (exports::alias(e, i).ok().as_deref() == Some(alias)).then_some(i))
        .ok_or_else(|| "Unknown export alias".into())
}
fn set_share(
    doc: &mut DocumentMut,
    config: &Config,
    alias: &str,
    access: Option<String>,
    public: Option<bool>,
) -> Result<(), String> {
    if access.is_none() && public.is_none() {
        return Err("share set requires --access or --public".into());
    }
    if let Some(mode) = &access {
        access_value(mode)?;
    }
    let row = tables(doc.as_table_mut(), "exports")?
        .get_mut(share_index(config, alias)?)
        .ok_or("Unknown export alias")?;
    if let Some(mode) = access {
        row["access"] = value(mode);
    }
    if let Some(enabled) = public {
        row["public"] = value(enabled);
    }
    Ok(())
}
fn access_value(access: &str) -> Result<(), String> {
    if matches!(access, "ro" | "rw") {
        Ok(())
    } else {
        Err("Access must be ro or rw".into())
    }
}
fn grant(
    doc: &mut DocumentMut,
    config: &Config,
    id: &str,
    alias: &str,
    access: &str,
    execute: bool,
) -> Result<(), String> {
    principal(config, id)?;
    access_value(access)?;
    let row = tables(doc.as_table_mut(), "exports")?
        .get_mut(share_index(config, alias)?)
        .ok_or("Unknown export alias")?;
    let rows = tables(row, "grants")?;
    let index = rows
        .iter()
        .position(|t| t.get("principal").and_then(Item::as_str) == Some(id));
    let mut entry = Table::new();
    entry["principal"] = value(id);
    entry["access"] = value(access);
    entry["execute"] = value(execute);
    if let Some(i) = index {
        *rows.get_mut(i).unwrap() = entry;
    } else {
        rows.push(entry);
    }
    Ok(())
}
fn revoke(doc: &mut DocumentMut, config: &Config, id: &str, alias: &str) -> Result<(), String> {
    principal(config, id)?;
    let row = tables(doc.as_table_mut(), "exports")?
        .get_mut(share_index(config, alias)?)
        .ok_or("Unknown export alias")?;
    let rows = tables(row, "grants")?;
    let existing = rows
        .iter()
        .position(|t| t.get("principal").and_then(Item::as_str) == Some(id));
    if let Some(index) = existing {
        rows.remove(index);
    }
    Ok(())
}
