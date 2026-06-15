//! The tool contract manifest (`herdr-tools.yaml`): the single source of truth for which
//! herdr methods are exposed, their group, description, and parameter schema.
//!
//! Compiled into the binary via `include_str!` — the tool surface is fixed at build time;
//! there is no runtime override.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{json, Map, Value};

const BUNDLED: &str = include_str!("../herdr-tools.yaml");

/// Name of the synthetic raw escape-hatch tool (not a herdr method).
pub const HERDR_CALL: &str = "herdr_call";

#[derive(Debug, Clone)]
pub struct MethodSpec {
    /// herdr socket method, e.g. `pane.send_input`. For the escape hatch this is `herdr_call`.
    pub method: String,
    /// MCP tool name = method with `.` -> `_`.
    pub tool_name: String,
    pub group: String,
    pub description: String,
    pub danger: bool,
    pub passthrough: bool,
    pub params: Vec<ParamSpec>,
}

#[derive(Debug, Clone)]
pub struct ParamSpec {
    pub name: String,
    pub ty: String,
    pub required: bool,
    pub desc: Option<String>,
    pub enum_values: Option<Vec<String>>,
}

// ── YAML shapes ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct RawManifest {
    #[serde(default)]
    herdr_version: String,
    methods: BTreeMap<String, RawMethod>,
}

#[derive(Deserialize)]
struct RawMethod {
    group: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    danger: bool,
    #[serde(default)]
    passthrough: bool,
    #[serde(default)]
    params: BTreeMap<String, RawParam>,
}

#[derive(Deserialize)]
struct RawParam {
    #[serde(rename = "type", default = "default_type")]
    ty: String,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    desc: Option<String>,
    #[serde(default, rename = "enum")]
    enum_values: Option<Vec<String>>,
}

fn default_type() -> String {
    "string".to_string()
}

// ── Loading ────────────────────────────────────────────────────────────────

/// Parse the build-time-embedded contract. The bundled YAML is validated by the
/// `bundled_manifest_parses_and_includes_escape_hatch` test, so a bad edit fails CI.
fn parse() -> RawManifest {
    serde_yaml::from_str(BUNDLED).expect("bundled herdr-tools.yaml: invalid YAML")
}

/// herdr version the contract targets (for the drift check).
pub fn herdr_version() -> String {
    parse().herdr_version
}

/// Load the contract into method specs, appending the `herdr_call` escape hatch.
pub fn load() -> Vec<MethodSpec> {
    let raw = parse();
    let mut specs: Vec<MethodSpec> = raw
        .methods
        .into_iter()
        .map(|(method, m)| {
            let params = m
                .params
                .into_iter()
                .map(|(name, p)| ParamSpec {
                    name,
                    ty: p.ty,
                    required: p.required,
                    desc: p.desc,
                    enum_values: p.enum_values,
                })
                .collect();
            MethodSpec {
                tool_name: method.replace('.', "_"),
                method,
                group: m.group,
                description: m.description,
                danger: m.danger,
                passthrough: m.passthrough,
                params,
            }
        })
        .collect();
    specs.push(escape_hatch());
    specs
}

fn escape_hatch() -> MethodSpec {
    MethodSpec {
        method: HERDR_CALL.to_string(),
        tool_name: HERDR_CALL.to_string(),
        group: "raw".to_string(),
        danger: true,
        passthrough: true,
        description: "Escape hatch: call any herdr socket method directly. Use for methods not \
                      otherwise exposed, or after a daemon upgrade adds methods."
            .to_string(),
        params: vec![
            ParamSpec {
                name: "method".to_string(),
                ty: "string".to_string(),
                required: true,
                desc: Some("herdr method, e.g. pane.list".to_string()),
                enum_values: None,
            },
            ParamSpec {
                name: "params".to_string(),
                ty: "object".to_string(),
                required: false,
                desc: Some("Params object for the method.".to_string()),
                enum_values: None,
            },
        ],
    }
}

/// Build a JSON Schema (object) for a method's input. Declared params yield a strict
/// schema (`additionalProperties:false`, so typos are rejected); `passthrough` methods
/// stay open.
pub fn input_schema(spec: &MethodSpec) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for p in &spec.params {
        let mut prop = Map::new();
        prop.insert("type".to_string(), Value::String(p.ty.clone()));
        if let Some(desc) = &p.desc {
            prop.insert("description".to_string(), Value::String(desc.clone()));
        }
        if let Some(values) = &p.enum_values {
            prop.insert(
                "enum".to_string(),
                Value::Array(values.iter().cloned().map(Value::String).collect()),
            );
        }
        properties.insert(p.name.clone(), Value::Object(prop));
        if p.required {
            required.push(Value::String(p.name.clone()));
        }
    }

    let mut schema = json!({
        "type": "object",
        "properties": Value::Object(properties),
        "additionalProperties": spec.passthrough,
    });
    if !required.is_empty() {
        schema["required"] = Value::Array(required);
    }
    schema
}

/// Validate call arguments against a method's contract, server-side (we don't rely on the
/// host to enforce the advertised schema). Returns a message on the first violation:
/// missing required field, unknown field (strict methods only), wrong type, or a value
/// outside an enum. `passthrough` methods skip the unknown-field check.
pub fn validate(spec: &MethodSpec, args: &Map<String, Value>) -> Result<(), String> {
    if !spec.passthrough {
        let known: std::collections::HashSet<&str> =
            spec.params.iter().map(|p| p.name.as_str()).collect();
        if let Some(key) = args.keys().find(|k| !known.contains(k.as_str())) {
            return Err(format!("unknown field `{key}`"));
        }
    }
    for p in &spec.params {
        match args.get(&p.name) {
            None if p.required => return Err(format!("missing required field `{}`", p.name)),
            None => {}
            Some(value) => {
                if !type_ok(&p.ty, value) {
                    return Err(format!("field `{}` must be {}", p.name, p.ty));
                }
                if let (Some(allowed), Some(s)) = (&p.enum_values, value.as_str()) {
                    if !allowed.iter().any(|a| a == s) {
                        return Err(format!(
                            "field `{}` must be one of [{}]",
                            p.name,
                            allowed.join(", ")
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

fn type_ok(ty: &str, value: &Value) -> bool {
    match ty {
        "string" => value.is_string(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        _ => true, // unknown declared type — don't block
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_manifest_parses_and_includes_escape_hatch() {
        let specs = load();
        assert!(specs.len() >= 60, "expected the full surface, got {}", specs.len());
        assert!(specs.iter().any(|s| s.tool_name == HERDR_CALL));
        assert!(specs.iter().any(|s| s.method == "pane.send_input"));
        // every spec has a non-empty group and tool name
        for s in &specs {
            assert!(!s.group.is_empty(), "{} has no group", s.method);
            assert!(!s.tool_name.is_empty());
        }
    }

    #[test]
    fn typed_method_schema_is_strict_with_required() {
        let specs = load();
        // a fully-enumerated method stays strict (additionalProperties:false)
        let send = specs.iter().find(|s| s.method == "agent.send").unwrap();
        let schema = input_schema(send);
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(schema["required"], json!(["target", "text"]));
        // enums are encoded straight from the source structs (SplitDirection = right|down)
        let split = specs.iter().find(|s| s.method == "pane.split").unwrap();
        assert_eq!(input_schema(split)["properties"]["direction"]["enum"], json!(["right", "down"]));
    }

    #[test]
    fn passthrough_method_schema_is_open() {
        let specs = load();
        let plugin = specs.iter().find(|s| s.method == "plugin.list").unwrap();
        assert_eq!(input_schema(plugin)["additionalProperties"], json!(true));
    }

    #[test]
    fn validate_enforces_required_unknown_and_enum() {
        let specs = load();
        let arg = |pairs: &[(&str, Value)]| pairs.iter().cloned().map(|(k, v)| (k.to_string(), v)).collect::<Map<String, Value>>();
        let find = |m: &str| specs.iter().find(|s| s.method == m).unwrap();

        // required + enum (pane.split: direction required, enum right|down)
        let split = find("pane.split");
        assert!(validate(split, &Map::new()).is_err(), "missing required direction");
        assert!(validate(split, &arg(&[("direction", json!("sideways"))])).is_err(), "bad enum");
        assert!(validate(split, &arg(&[("direction", json!("right"))])).is_ok(), "valid split");

        // unknown-field rejection on a strict (fully-enumerated) method
        let send = find("agent.send");
        assert!(validate(send, &arg(&[("target", json!("x")), ("text", json!("y")), ("typo", json!(1))])).is_err(), "unknown field");
        assert!(validate(send, &arg(&[("target", json!("x")), ("text", json!("y"))])).is_ok(), "valid send");

        // passthrough allows extras
        let plugin = find("plugin.list");
        assert!(validate(plugin, &arg(&[("whatever", json!(true))])).is_ok(), "passthrough allows extras");
    }

    #[test]
    fn wait_methods_have_expected_contract() {
        let specs = load();
        let arg = |pairs: &[(&str, Value)]| pairs.iter().cloned().map(|(k, v)| (k.to_string(), v)).collect::<Map<String, Value>>();

        // events.wait requires match_event (not subscriptions), plus optional timeout_ms.
        let ew = specs.iter().find(|s| s.method == "events.wait").unwrap();
        assert!(validate(ew, &Map::new()).is_err(), "missing match_event");
        assert!(validate(ew, &arg(&[("match_event", json!({"event": "pane_agent_status_changed"})), ("timeout_ms", json!(5000))])).is_ok());

        // pane.wait_for_output requires pane_id + source + match (a structured object).
        let wfo = specs.iter().find(|s| s.method == "pane.wait_for_output").unwrap();
        assert!(validate(wfo, &Map::new()).is_err(), "missing required");
        assert!(validate(wfo, &arg(&[
            ("pane_id", json!("w1:p1")),
            ("source", json!("recent")),
            ("match", json!({"type": "substring", "value": "done"})),
        ])).is_ok());
    }

    /// Drift check: the manifest's methods must exactly match what the live daemon
    /// accepts. Run with `cargo test -- --ignored` against a running herdr.
    #[test]
    #[ignore = "requires a live herdr daemon"]
    fn manifest_matches_live_daemon() {
        use std::collections::HashSet;
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixStream;

        let sock = std::path::PathBuf::from(std::env::var("HOME").unwrap())
            .join(".config/herdr/herdr.sock");
        let mut stream = UnixStream::connect(&sock).expect("connect herdr socket");
        stream
            .write_all(br#"{"id":"d","method":"__drift_probe__","params":{}}"#)
            .unwrap();
        stream.write_all(b"\n").unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        let msg = v["error"]["message"].as_str().unwrap_or("");

        // The daemon's "unknown variant" error lists every valid method in backticks.
        let live: HashSet<String> = msg
            .split('`')
            .map(|t| t.trim())
            .filter(|t| t.contains('.') || *t == "ping")
            .map(|s| s.to_string())
            .collect();
        let ours: HashSet<String> = load()
            .into_iter()
            .filter(|m| m.tool_name != HERDR_CALL)
            .map(|m| m.method)
            .collect();

        let missing: Vec<_> = live.difference(&ours).collect();
        let extra: Vec<_> = ours.difference(&live).collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "manifest drift — in daemon but not manifest: {missing:?}; in manifest but not daemon: {extra:?}"
        );
    }
}
