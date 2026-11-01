use crate::admin::{AccessCommand as Command, Credential};
use std::{collections::BTreeMap, path::PathBuf};

pub(super) const HELP: &str = "Directory policy administration (local configuration only):
  cells access [--config CONFIG] [--dry-run] <command>
  init                                    Migrate legacy credentials to principal owner
  list                                    Show identities and effective directory permissions
  validate                                Validate configured auth and directory policy
  check PRINCIPAL ALIAS [read|write|execute] Exit nonzero if denied; anonymous is supported
  principal add ID --token-env ENV
  principal add ID --username USER --password-env ENV
  principal credential ID <same credential options>
  principal remove ID                      Refuse referenced or last identities
  share add ALIAS ABSOLUTE_PATH [ro|rw] [--public true|false]
  share set ALIAS [--access ro|rw] [--public true|false]
  share remove ALIAS                       Remove sharing, never delete directory contents
  grant PRINCIPAL ALIAS ro|rw [--execute]    Replace that grant; execution is independent
  revoke PRINCIPAL ALIAS                   Remove explicit grant; public reads still apply

CONFIG defaults to normal server discovery. Credential environment variables must be set.
Edits validate before atomic save, retain a private backup, and require a server restart.
--dry-run validates without writing. Output is JSON; no credential values are printed.";

const VALUE_OPTIONS: &[&str] = &[
    "config",
    "token-env",
    "username",
    "password-env",
    "public",
    "access",
];
const INVALID: &str = "Invalid access command; use access --help";
type Options = BTreeMap<String, String>;

pub(super) fn parse(args: &[String]) -> Result<(PathBuf, Command, bool), String> {
    let (words, mut options) = arguments(args)?;
    let path = crate::config::launch::resolve(options.remove("config").as_deref())?;
    let dry_run = options.remove("dry-run").is_some();
    let command = command(&words, &mut options)?;
    if !options.is_empty() {
        return Err("Unexpected option for this command; use access --help".into());
    }
    Ok((path, command, dry_run))
}
fn arguments(args: &[String]) -> Result<(Vec<&str>, Options), String> {
    let mut words = Vec::new();
    let mut options = Options::new();
    let mut input = args.iter();
    while let Some(arg) = input.next() {
        let Some(key) = arg.strip_prefix("--") else {
            words.push(arg.as_str());
            continue;
        };
        let value = if ["dry-run", "execute"].contains(&key) {
            "true"
        } else {
            if !VALUE_OPTIONS.contains(&key) {
                return Err("Unknown option; use access --help".into());
            }
            input
                .next()
                .filter(|s| !s.starts_with("--"))
                .ok_or("Missing option value")?
        };
        if options.insert(key.into(), value.into()).is_some() {
            return Err("Duplicate option".into());
        }
    }
    Ok((words, options))
}
fn command(words: &[&str], options: &mut Options) -> Result<Command, String> {
    match words {
        ["init"] => Ok(Command::Init),
        ["list"] => Ok(Command::List),
        ["validate"] => Ok(Command::Validate),
        ["check", rest @ ..] => check(rest),
        ["principal", rest @ ..] => principal(rest, options),
        ["share", rest @ ..] => share(rest, options),
        ["grant", principal, alias, access] => Ok(Command::Grant {
            principal: (*principal).into(),
            alias: (*alias).into(),
            access: (*access).into(),
            execute: options.remove("execute").is_some(),
        }),
        ["revoke", principal, alias] => Ok(Command::Revoke {
            principal: (*principal).into(),
            alias: (*alias).into(),
        }),
        _ => Err(INVALID.into()),
    }
}
fn check(words: &[&str]) -> Result<Command, String> {
    if ![2, 3].contains(&words.len()) {
        return Err(INVALID.into());
    }
    Ok(Command::Check {
        principal: words[0].into(),
        alias: words[1].into(),
        operation: words.get(2).copied().unwrap_or("read").into(),
    })
}
fn principal(words: &[&str], options: &mut Options) -> Result<Command, String> {
    let [action, id] = words else {
        return Err(INVALID.into());
    };
    match *action {
        "add" => Ok(Command::AddPrincipal {
            id: (*id).into(),
            credential: credential(options)?,
        }),
        "credential" => Ok(Command::SetCredential {
            id: (*id).into(),
            credential: credential(options)?,
        }),
        "remove" => Ok(Command::RemovePrincipal { id: (*id).into() }),
        _ => Err(INVALID.into()),
    }
}
fn share(words: &[&str], options: &mut Options) -> Result<Command, String> {
    match words {
        ["add", alias, path] => add_share(alias, path, "ro", options),
        ["add", alias, path, access] => add_share(alias, path, access, options),
        ["set", alias] => Ok(Command::SetShare {
            alias: (*alias).into(),
            access: options.remove("access"),
            public: boolean(options.remove("public"))?,
        }),
        ["remove", alias] => Ok(Command::RemoveShare {
            alias: (*alias).into(),
        }),
        _ => Err(INVALID.into()),
    }
}
fn add_share(
    alias: &str,
    path: &str,
    access: &str,
    options: &mut Options,
) -> Result<Command, String> {
    Ok(Command::AddShare {
        alias: alias.into(),
        path: path.into(),
        access: access.into(),
        public: boolean(options.remove("public"))?.unwrap_or(false),
    })
}
fn credential(options: &mut Options) -> Result<Credential, String> {
    match (
        options.remove("token-env"),
        options.remove("username"),
        options.remove("password-env"),
    ) {
        (Some(environment), None, None) => Ok(Credential::Bearer { environment }),
        (None, Some(username), Some(environment)) => Ok(Credential::Basic {
            username,
            environment,
        }),
        _ => Err("Use exactly --token-env ENV or --username USER --password-env ENV".into()),
    }
}
fn boolean(value: Option<String>) -> Result<Option<bool>, String> {
    value
        .map(|s| match s.as_str() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err("--public must be true or false".into()),
        })
        .transpose()
}
