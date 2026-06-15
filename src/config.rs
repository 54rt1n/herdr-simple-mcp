//! Tool-surface configuration.
//!
//! Methods carry a capability **group** (from the manifest). A **profile** is a named
//! bundle of groups. Env selects the surface; the server disables every excluded tool.
//!
//! - `HERDR_MCP_PROFILE`  — `full` (default) | `coordinator` | `client` | `observer`
//! - `HERDR_MCP_GROUPS`   — explicit comma list of groups (overrides the profile)
//! - `HERDR_MCP_ALLOW`    — comma list of methods / `prefix*` globs to add
//! - `HERDR_MCP_DENY`     — comma list of methods / `prefix*` globs to remove (wins)
//!
//! Globs match the dotted method (`pane.*`) or the tool name (`pane_*`).
//! Note: the `raw` group (the `herdr_call` escape hatch) is **opt-in** — excluded from
//! every profile, including `full`. Enable it via `HERDR_MCP_GROUPS=…,raw` or
//! `HERDR_MCP_ALLOW=herdr_call`. When enabled it still applies deny patterns and refuses
//! known methods disabled by the profile (see `guard_reason` in `server.rs`), but it
//! cannot group-classify methods absent from the manifest.

use std::collections::HashSet;

use crate::manifest::MethodSpec;

fn glob_match(pat: &str, target: &str) -> bool {
    match pat.strip_suffix('*') {
        Some(prefix) => target.starts_with(prefix),
        None => target == pat,
    }
}

fn matches(pat: &str, spec: &MethodSpec) -> bool {
    let pat = pat.trim();
    glob_match(pat, &spec.method) || glob_match(pat, &spec.tool_name)
}

fn split_list(value: Option<String>) -> Vec<String> {
    value
        .map(|s| {
            s.split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn profile_groups(profile: &str, all_groups: &HashSet<String>) -> HashSet<String> {
    let pick = |groups: &[&str]| groups.iter().map(|s| s.to_string()).collect::<HashSet<_>>();
    // `full` is every group EXCEPT `raw` (the herdr_call escape hatch), which is opt-in
    // because it bypasses per-method exposure.
    let full = || {
        all_groups
            .iter()
            .filter(|g| g.as_str() != "raw")
            .cloned()
            .collect::<HashSet<_>>()
    };
    match profile.trim().to_ascii_lowercase().as_str() {
        "full" => full(),
        "coordinator" => pick(&["observe", "structure", "panes", "io", "agents", "events"]),
        "client" => pick(&["observe", "io", "authority", "events"]),
        "observer" => pick(&["observe"]),
        other => {
            // Fail closed: an unknown/typoed profile must not silently expose everything.
            tracing::error!("unknown HERDR_MCP_PROFILE={other:?}; failing closed to 'observer' (read-only)");
            pick(&["observe"])
        }
    }
}

/// Pure resolution: groups(profile or GROUPS) -> +allow -> -deny. Returns enabled methods.
fn resolve(
    methods: &[MethodSpec],
    profile: &str,
    groups_env: Option<&str>,
    allow: &[String],
    deny: &[String],
) -> HashSet<String> {
    let all_groups: HashSet<String> = methods.iter().map(|m| m.group.clone()).collect();

    let groups: HashSet<String> = match groups_env {
        Some(s) if !s.trim().is_empty() => s
            .split(',')
            .map(|x| x.trim().to_ascii_lowercase())
            .filter(|x| !x.is_empty())
            .collect(),
        _ => profile_groups(profile, &all_groups),
    };

    let mut enabled: HashSet<String> = methods
        .iter()
        .filter(|m| groups.contains(&m.group))
        .map(|m| m.method.clone())
        .collect();

    for pat in allow {
        for m in methods {
            if matches(pat, m) {
                enabled.insert(m.method.clone());
            }
        }
    }
    for pat in deny {
        for m in methods {
            if matches(pat, m) {
                enabled.remove(&m.method);
            }
        }
    }
    enabled
}

pub fn enabled_methods(methods: &[MethodSpec]) -> HashSet<String> {
    let profile = std::env::var("HERDR_MCP_PROFILE").unwrap_or_else(|_| "full".to_string());
    let groups_env = std::env::var("HERDR_MCP_GROUPS").ok();
    let allow = split_list(std::env::var("HERDR_MCP_ALLOW").ok());
    let deny = split_list(std::env::var("HERDR_MCP_DENY").ok());
    resolve(methods, &profile, groups_env.as_deref(), &allow, &deny)
}

/// The configured `HERDR_MCP_DENY` patterns (for enforcing policy inside `herdr_call`).
pub fn deny_patterns() -> Vec<String> {
    split_list(std::env::var("HERDR_MCP_DENY").ok())
}

/// Whether any deny pattern matches a method (by dotted name or tool name).
pub fn deny_matches(patterns: &[String], method: &str) -> bool {
    let tool_name = method.replace('.', "_");
    patterns
        .iter()
        .any(|pat| glob_match(pat.trim(), method) || glob_match(pat.trim(), &tool_name))
}

/// One-line summary for the startup log.
pub fn summary(methods: &[MethodSpec]) -> String {
    format!("{}/{} tools enabled", enabled_methods(methods).len(), methods.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn full_enables_everything_but_raw() {
        let m = manifest::load();
        let e = resolve(&m, "full", None, &[], &[]);
        assert_eq!(e.len(), m.len() - 1); // all but the herdr_call escape hatch
        assert!(!e.contains("herdr_call"));
    }

    #[test]
    fn client_is_narrow() {
        let m = manifest::load();
        let e = resolve(&m, "client", None, &[], &[]);
        assert!(e.contains("pane.send_input")); // io
        assert!(e.contains("pane.read")); // observe
        assert!(e.contains("pane.report_agent")); // authority
        assert!(e.contains("events.wait")); // events
        assert!(!e.contains("workspace.create")); // structure
        assert!(!e.contains("agent.start")); // agents
        assert!(!e.contains("server.stop")); // admin
        assert!(!e.contains("herdr_call")); // raw escape hatch excluded
    }

    #[test]
    fn coordinator_has_agents_not_admin_or_raw() {
        let m = manifest::load();
        let e = resolve(&m, "coordinator", None, &[], &[]);
        assert!(e.contains("agent.start"));
        assert!(e.contains("workspace.create"));
        assert!(!e.contains("server.stop"));
        assert!(!e.contains("pane.report_agent"));
        assert!(!e.contains("herdr_call"));
    }

    #[test]
    fn raw_escape_hatch_is_opt_in() {
        let m = manifest::load();
        assert!(!resolve(&m, "full", None, &[], &[]).contains("herdr_call"));
        assert!(!resolve(&m, "observer", None, &[], &[]).contains("herdr_call"));
        // opt in explicitly via ALLOW or the raw group
        assert!(resolve(&m, "observer", None, &s(&["herdr_call"]), &[]).contains("herdr_call"));
        assert!(resolve(&m, "full", Some("raw"), &[], &[]).contains("herdr_call"));
    }

    #[test]
    fn unknown_profile_fails_closed_to_observer() {
        let m = manifest::load();
        let e = resolve(&m, "obsever", None, &[], &[]);
        assert!(e.contains("workspace.list")); // observe
        assert!(!e.contains("workspace.create")); // not structure
        assert!(!e.contains("server.stop")); // not admin
    }

    #[test]
    fn groups_override_and_deny_wins() {
        let m = manifest::load();
        let e = resolve(&m, "full", Some("observe"), &[], &[]);
        assert!(e.contains("workspace.list"));
        assert!(!e.contains("workspace.create"));

        let e = resolve(&m, "full", None, &s(&["server.stop"]), &s(&["server.*"]));
        assert!(!e.contains("server.stop"));
        assert!(!e.contains("server.reload_config"));

        // underscore-form glob also matches
        let e = resolve(&m, "full", None, &[], &s(&["plugin_*"]));
        assert!(!e.contains("plugin.list"));
        assert!(!e.contains("plugin.enable"));
    }
}
