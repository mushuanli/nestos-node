//! Explicit host profiles; no executable or host path comes from a remote request.
use crate::{
    config::{Access, Config},
    core::ids,
};
use serde::Deserialize;
use std::{collections::HashSet, path::PathBuf};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileConfig {
    pub id: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub kind: String,
    #[serde(default = "codex")]
    pub command: String,
    pub home: PathBuf,
    pub principal_id: Option<String>,
    #[serde(default)]
    pub projects: bool,
    #[serde(default)]
    pub workspaces: Vec<WorkspaceConfig>,
    pub dsh: Option<super::dsh::config::DshConfig>,
    pub codex: Option<CodexConfig>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodexConfig {
    pub reasoning_summary: Option<ReasoningSummary>,
}

#[derive(Clone, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningSummary {
    Auto,
    Concise,
    Detailed,
    None,
}

#[cfg(test)]
mod summary_tests {
    use super::CodexConfig;
    #[test]
    fn rejects_unknown_summary_modes_and_misspelled_fields() {
        assert!(toml::from_str::<CodexConfig>("reasoning_summary = 'automatic'").is_err());
        assert!(toml::from_str::<CodexConfig>("reasoning_summaries = 'auto'").is_err());
        assert!(toml::from_str::<CodexConfig>("reasoning_summary = 'auto'").is_ok());
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    pub id: String,
    pub path: PathBuf,
}

fn codex() -> String {
    "codex".into()
}

pub fn validate(config: &Config) -> Result<Vec<ProfileConfig>, String> {
    validate_with_plugins(config, &super::HarnessPlugins::default())
}

pub fn validate_with_plugins(
    config: &Config,
    plugins: &super::HarnessPlugins,
) -> Result<Vec<ProfileConfig>, String> {
    let mut profiles = config.harnesses.clone();
    let mut ids = HashSet::new();
    for profile in &mut profiles {
        if profile.command.is_empty()
            || profile.aliases.len() > 128
            || std::iter::once(&profile.id)
                .chain(profile.aliases.iter())
                .any(|id| !ids::is_identifier(id) || !ids.insert(id.clone()))
        {
            return Err("Invalid or duplicate harness profile".into());
        }
        profile.home = directory(&profile.home)?;
        let mut workspaces = HashSet::new();
        for workspace in &mut profile.workspaces {
            if !workspaces.insert(workspace.id.clone()) || !ids::is_identifier(&workspace.id) {
                return Err("Invalid or duplicate harness workspace".into());
            }
            workspace.path = directory(&workspace.path)?;
            reject_exclusive(config, &workspace.path)?;
        }
        plugins.validate(profile)?;
    }
    Ok(profiles)
}

fn directory(path: &std::path::Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("Harness paths must be absolute".into());
    }
    let path = path
        .canonicalize()
        .map_err(|_| "Harness directory unavailable")?;
    if !path.is_dir() {
        return Err("Harness path is not a directory".into());
    }
    Ok(path)
}

fn reject_exclusive(config: &Config, path: &std::path::Path) -> Result<(), String> {
    for export in config.exports.iter().filter(|e| e.access == Access::Rw) {
        let root = std::path::Path::new(&export.path)
            .canonicalize()
            .map_err(|_| "Export unavailable")?;
        if path.starts_with(&root) || root.starts_with(path) {
            return Err("Harness workspaces must not overlap exclusive writable exports".into());
        }
    }
    Ok(())
}
