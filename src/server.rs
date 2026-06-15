//! The MCP server: tools are built dynamically from the manifest (the single source of
//! truth), so each gets a real input schema. The server holds no state — herdr's daemon
//! is the authority.
//!
//! Each call is validated against its contract server-side (we don't trust the host to
//! enforce the advertised schema) and then forwarded through [`call`], which returns a
//! structured result. `herdr_call` additionally enforces the configured deny/profile
//! policy on its target method so it cannot be used to bypass the surface.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;

use rmcp::{
    handler::server::router::tool::{ToolRoute, ToolRouter},
    handler::server::tool::ToolCallContext,
    model::{self, CallToolResult, Content, Tool},
    tool_handler, ErrorData as McpError, ServerHandler,
};
use serde_json::{json, Value};

use crate::manifest::{self, MethodSpec, HERDR_CALL};
use crate::{config, herdr};

#[derive(Clone)]
pub struct HerdrMcpServer {
    tool_router: ToolRouter<HerdrMcpServer>,
}

impl HerdrMcpServer {
    /// Build the server: one tool per manifest method, with the configured surface
    /// (see [`crate::config`]) applied by disabling excluded tools.
    pub fn new() -> Self {
        let methods = manifest::load();
        let enabled = config::enabled_methods(&methods);
        tracing::info!(
            "tool surface: {} (herdr manifest {})",
            config::summary(&methods),
            manifest::herdr_version()
        );

        // Data captured by the herdr_call guard so it honors the same policy.
        let known: HashSet<String> = methods
            .iter()
            .filter(|m| m.tool_name != HERDR_CALL)
            .map(|m| m.method.clone())
            .collect();
        let deny = config::deny_patterns();

        let mut tool_router = ToolRouter::new();
        for spec in &methods {
            let route = if spec.tool_name == HERDR_CALL {
                build_escape_route(spec, enabled.clone(), known.clone(), deny.clone())
            } else {
                build_route(spec)
            };
            tool_router.add_route(route);
        }
        for spec in &methods {
            if !enabled.contains(&spec.method) {
                tool_router.disable_route(spec.tool_name.clone());
            }
        }
        Self { tool_router }
    }
}

type BoxFut = Pin<Box<dyn Future<Output = Result<CallToolResult, McpError>> + Send>>;

fn annotations(read_only: bool, destructive: bool) -> model::ToolAnnotations {
    model::ToolAnnotations::new()
        .read_only(read_only)
        .destructive(destructive)
}

/// A normal method route: validate args against the contract, then forward.
fn build_route(spec: &MethodSpec) -> ToolRoute<HerdrMcpServer> {
    let read_only = spec.group == "observe";
    let mut tool = Tool::new(
        spec.tool_name.clone(),
        spec.description.clone(),
        model::object(manifest::input_schema(spec)),
    );
    // Anything that isn't read-only may modify herdr state — flag it destructive so hosts
    // can gate it (the `danger` flag is a subset, kept explicit).
    tool.annotations = Some(annotations(read_only, !read_only || spec.danger));

    let method = spec.method.clone();
    let spec = spec.clone();
    ToolRoute::new_dyn(tool, move |ctx: ToolCallContext<'_, HerdrMcpServer>| {
        let args = ctx.arguments.unwrap_or_default();
        if let Err(msg) = manifest::validate(&spec, &args) {
            let method = method.clone();
            return Box::pin(async move {
                Ok(error_result("request", "invalid_params", &msg, &method))
            }) as BoxFut;
        }
        let method = method.clone();
        Box::pin(async move { Ok(call(&method, Value::Object(args)).await) }) as BoxFut
    })
}

/// The `herdr_call` escape hatch: validate its own args, enforce the deny/profile policy
/// on the *target* method (so it can't bypass the configured surface), then forward.
fn build_escape_route(
    spec: &MethodSpec,
    enabled: HashSet<String>,
    known: HashSet<String>,
    deny: Vec<String>,
) -> ToolRoute<HerdrMcpServer> {
    let mut tool = Tool::new(
        spec.tool_name.clone(),
        spec.description.clone(),
        model::object(manifest::input_schema(spec)),
    );
    tool.annotations = Some(annotations(false, true));

    let spec = spec.clone();
    ToolRoute::new_dyn(tool, move |ctx: ToolCallContext<'_, HerdrMcpServer>| {
        let args = ctx.arguments.unwrap_or_default();
        if let Err(msg) = manifest::validate(&spec, &args) {
            return Box::pin(async move {
                Ok(error_result("request", "invalid_params", &msg, HERDR_CALL))
            }) as BoxFut;
        }
        let inner = args
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if let Some(reason) = guard_reason(&inner, &enabled, &known, &deny) {
            return Box::pin(async move { Ok(error_result("policy", "forbidden", &reason, &inner)) })
                as BoxFut;
        }
        let params = args.get("params").cloned().unwrap_or_else(|| json!({}));
        Box::pin(async move { Ok(call(&inner, params).await) }) as BoxFut
    })
}

/// Why a `herdr_call` target is refused, or `None` if allowed. Permits enabled methods and
/// unknown (drift) methods; refuses denied methods and methods disabled by the profile.
fn guard_reason(
    method: &str,
    enabled: &HashSet<String>,
    known: &HashSet<String>,
    deny: &[String],
) -> Option<String> {
    if config::deny_matches(deny, method) {
        return Some(format!("`{method}` is blocked by HERDR_MCP_DENY"));
    }
    if known.contains(method) && !enabled.contains(method) {
        return Some(format!(
            "`{method}` is disabled by the current profile; herdr_call cannot bypass it"
        ));
    }
    None
}

/// Issue one herdr socket request and wrap the outcome. Failures become an `is_error`
/// result carrying structured JSON `{error:{kind,code,message,method}}`.
async fn call(method: &str, params: Value) -> CallToolResult {
    match herdr::request(method, params).await {
        Ok(value) => match Content::json(value) {
            Ok(content) => CallToolResult::success(vec![content]),
            Err(e) => error_result("transport", "serialize", &e.to_string(), method),
        },
        Err(herdr::HerdrError::Api { code, message }) => error_result("api", &code, &message, method),
        Err(herdr::HerdrError::Transport(message)) => {
            error_result("transport", "transport", &message, method)
        }
    }
}

fn error_result(kind: &str, code: &str, message: &str, method: &str) -> CallToolResult {
    let body = json!({
        "error": { "kind": kind, "code": code, "message": message, "method": method }
    });
    match Content::json(body) {
        Ok(content) => CallToolResult::error(vec![content]),
        Err(_) => CallToolResult::error(vec![Content::text(format!(
            "{kind}/{code}: {message} ({method})"
        ))]),
    }
}

#[tool_handler(
    router = self.tool_router,
    name = "herdr-simple-mcp",
    version = "0.7.0",
    instructions = "A transparent bridge to herdr's local Unix-socket API. Tools are named \
                    after herdr methods (dots -> underscores); each has a typed input \
                    schema where the contract is known, or accepts an open params object \
                    otherwise. Structure: workspaces contain tabs, tabs contain panes, \
                    panes may run agents. Scope ids (workspace_id/tab_id/pane_id) are \
                    mostly optional — omit to use the focused context. Agents are \
                    addressed by `target` (terminal_id, pane_id, or unambiguous agent \
                    name). IDs are session-local and may compact when items close. herdr \
                    must be running; the socket is found via HERDR_SOCKET_PATH, \
                    HERDR_SESSION, or ~/.config/herdr/herdr.sock."
)]
impl ServerHandler for HerdrMcpServer {}
