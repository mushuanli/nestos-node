//! Connection hints; the entry point separately prints the configured API Key.
use cells::{app::State, config::Config};
use serde::Serialize;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};

pub fn print(config: &Config, bound: SocketAddr, state: &State) {
    let mut effective = config.clone();
    effective.plugins = state.plugins.statuses().clone();
    effective.execution = state.plugins.enabled("process");
    if !state.plugins.enabled("projects") {
        effective.projects = None;
    }
    if !state.plugins.enabled("sync") {
        effective.sync = None;
    }
    if !state.plugins.enabled("harness") {
        effective.harnesses.clear();
    }
    eprintln!(
        "{}",
        summary(&effective, bound, state.auth.server_id(), route_ip(bound))
    );
    eprintln!("  Plugins: {}", serde_json::json!(state.plugins.statuses()));
    for export in export_access(state) {
        eprintln!(
            "  Export /{}: {} (read={}, write={})",
            export.alias, export.access, export.read, export.write
        );
    }
    if !state.exports.is_empty() {
        eprintln!("  Project access: saved per project; client bindings may further restrict exports to ro");
    }
}

#[derive(Serialize)]
pub struct ExportAccess<'a> {
    alias: &'a str,
    access: &'static str,
    read: bool,
    write: bool,
}

pub fn export_access(state: &State) -> Vec<ExportAccess<'_>> {
    let client = state.auth.client(0);
    state
        .exports
        .iter()
        .map(|(alias, export)| {
            let read = client.may_read(alias);
            let write = read && client.may_write(alias) && export.writable();
            ExportAccess {
                alias,
                access: if write { "rw" } else { "ro" },
                read,
                write,
            }
        })
        .collect()
}

fn summary(
    config: &Config,
    bound: SocketAddr,
    server_id: Option<&str>,
    route: Option<IpAddr>,
) -> String {
    let local = if bound.ip().is_unspecified() {
        if bound.is_ipv4() {
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        } else {
            IpAddr::V6(Ipv6Addr::LOCALHOST)
        }
    } else {
        bound.ip()
    };
    let mut lines = vec![
        format!("  Server ID: {}", server_id.unwrap_or("not configured")),
        format!(
            "  MCP endpoint: {}",
            if config.plugins.get("mcp") == Some(&false) {
                "disabled".into()
            } else {
                endpoint(SocketAddr::new(local, bound.port()))
            }
        ),
        format!("  Authentication: {}", authentication(config)),
        format!(
            "  Features: projects={}, harness={}, sync={}",
            config.projects.is_some(),
            !config.harnesses.is_empty(),
            config.sync.as_ref().is_some_and(|s| s.enabled)
        ),
    ];
    if bound.ip().is_unspecified() && config.plugins.get("mcp") != Some(&false) {
        lines.push(
            match route.filter(|ip| !ip.is_loopback() && !ip.is_unspecified()) {
                Some(ip) => format!(
                    "  Remote MCP candidate: {} (verify client reachability)",
                    endpoint(SocketAddr::new(ip, bound.port()))
                ),
                None => format!("  Remote MCP: http://<server-ip>:{}/mcp", bound.port()),
            },
        );
    }
    lines.join("\n")
}

fn endpoint(address: SocketAddr) -> String {
    format!("http://{address}/mcp")
}

fn authentication(config: &Config) -> String {
    if !config.principals.is_empty() {
        return format!(
            "{} configured principals; anonymous read={}",
            config.principals.len(),
            config.exports.iter().any(|e| e.public)
        );
    }
    if config.password.is_some() || config.password_env.is_some() {
        let username = config
            .username
            .clone()
            .or_else(|| std::env::var("FS_SERVER_USER").ok())
            .unwrap_or_default();
        let source = config
            .password_env
            .as_deref()
            .map(|name| format!("environment variable {name}"))
            .unwrap_or_else(|| "config field password".into());
        format!("Basic; username={username}; password from {source}")
    } else {
        let source = config
            .token_env
            .as_deref()
            .map(|name| format!("environment variable {name}"))
            .unwrap_or_else(|| "config field api_key (legacy: token)".into());
        format!("Bearer (MCP API Key); token from {source}")
    }
}

fn route_ip(bound: SocketAddr) -> Option<IpAddr> {
    if !bound.ip().is_unspecified() {
        return None;
    }
    // UDP connect selects a source address without sending a datagram.
    let (local, destination) = if bound.is_ipv4() {
        ("0.0.0.0:0", "192.0.2.1:9")
    } else {
        ("[::]:0", "[2001:db8::1]:9")
    };
    let socket = UdpSocket::bind(local).ok()?;
    socket.connect(destination).ok()?;
    Some(socket.local_addr().ok()?.ip())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(auth: &str) -> Config {
        toml::from_str(&format!("listen='0.0.0.0:8787'\n{auth}")).unwrap()
    }

    #[test]
    fn wildcard_bind_shows_connectable_candidates_without_disclosing_password() {
        let config = config("username='li'\npassword='private-password-value'");
        let text = summary(
            &config,
            "0.0.0.0:8787".parse().unwrap(),
            Some("node"),
            Some("192.168.1.12".parse().unwrap()),
        );
        assert!(text.contains("http://127.0.0.1:8787/mcp"));
        assert!(text.contains("http://192.168.1.12:8787/mcp"));
        assert!(text.contains("Basic; username=li; password from config field password"));
        assert!(!text.contains("private-password-value"));
        assert!(!text.contains("http://0.0.0.0"));
    }

    #[test]
    fn specific_ipv6_bind_preserves_address_and_does_not_advertise_other_interfaces() {
        let config = config("token='private-token-value-at-least-24-bytes'");
        let text = summary(
            &config,
            "[::1]:8787".parse().unwrap(),
            None,
            Some("192.168.1.12".parse().unwrap()),
        );
        assert!(text.contains("http://[::1]:8787/mcp"));
        assert!(text.contains("Bearer (MCP API Key); token from config field api_key"));
        assert!(!text.contains("private-token-value"));
        assert!(!text.contains("Remote MCP"));
    }

    #[test]
    fn environment_authentication_reports_only_the_reference() {
        let config = config("token_env='CELLS_TOKEN'");
        let text = summary(&config, "0.0.0.0:43210".parse().unwrap(), None, None);
        assert!(text.contains("token from environment variable CELLS_TOKEN"));
        assert!(text.contains("<server-ip>"));
        assert!(text.contains(":43210"));
    }

    fn permission_fixture() -> (Vec<tempfile::TempDir>, std::sync::Arc<State>) {
        use cells::{
            auth::{Auth, Client},
            fs::{Export, Exports},
        };
        use std::{collections::BTreeMap, sync::Arc};
        let roots: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
        let exports = Exports::new(
            ["docs", "limited", "work"]
                .into_iter()
                .zip(&roots)
                .enumerate()
                .map(|(index, (alias, root))| {
                    let export = if index == 0 {
                        Export::open(root.path())
                    } else {
                        Export::exclusive(root.path())
                    };
                    (alias.into(), Arc::new(export.unwrap()))
                })
                .collect::<BTreeMap<_, _>>(),
        );
        let client = Client::new(
            "private-token".into(),
            None,
            vec!["docs".into(), "limited".into(), "work".into()],
            vec!["work".into()],
        );
        let state = State::new(Auth::new(None, vec![client]), exports).unwrap();
        (roots, state)
    }

    #[test]
    fn reports_opened_export_modes_and_the_principal_write_limit() {
        let (_roots, state) = permission_fixture();
        let result = serde_json::to_value(export_access(&state)).unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                {"alias":"docs", "access":"ro", "read":true, "write":false},
                {"alias":"limited", "access":"ro", "read":true, "write":false},
                {"alias":"work", "access":"rw", "read":true, "write":true}
            ])
        );
        assert!(!result.to_string().contains("private-token"));
    }
}
