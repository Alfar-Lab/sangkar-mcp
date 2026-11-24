//! Phase 0 spike (TASK-011): can rmcp 3.11's stdio transport reproduce the TypeScript server's
//! wire behaviour? It serves one tool named `echo` over `rmcp`, and answers nothing else — the
//! recorder in `scripts/parity/mcp-session.ts` replays a session recorded from `sangkar mcp` against
//! this binary, and `DECISION.md` records what came back.
//!
//! Nothing here is meant to ship: the package is standalone (`[workspace]` in `Cargo.toml`), the
//! tool set is one, and the schema is the raw JSON object in `echo_schema.json` rather than a
//! `schemars` derive — because the product's seven schemas are literals in `src/tools/*/manifest.ts`
//! and the question is whether a raw object survives `tools/list` unchanged.
//!
//! stdout carries the protocol and nothing else. `rmcp` logs through `tracing`, and no subscriber is
//! installed, so there is no second writer to keep off stdout.

use std::sync::Arc;

use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig,
        Tool,
    },
    service::RequestContext,
    transport::stdio,
};
use serde_json::Value;

const TOOL_NAME: &str = "echo";
const TOOL_DESCRIPTION: &str = "Return the arguments it was called with, as JSON.";
const INSTRUCTIONS: &str = "Spike server: one tool, `echo`, which returns its arguments as JSON.";

/// The tool's `inputSchema`, spelled as the raw JSON the product's manifests would hand it — key
/// order, `$schema` and `additionalProperties` included, none of which a derive macro would produce.
/// `tools/list` must return exactly this object (question (d)). It lives in its own file so the
/// recorder compares the *declared* object against the one on the wire instead of restating it.
const RAW_INPUT_SCHEMA: &str = include_str!("../echo_schema.json");

struct Echo;

impl Echo {
    fn schema() -> Arc<JsonObject> {
        let parsed: JsonObject =
            serde_json::from_str(RAW_INPUT_SCHEMA).expect("the raw schema is a JSON object");
        Arc::new(parsed)
    }
}

/// The one thing the transport cannot do for a server: `rmcp` does not validate `arguments` against
/// `inputSchema` (the schema is an opaque object it forwards), so the handler owns the check. This
/// is the minimum that lets the recording answer question (c) — a `text` that is not a string, or a
/// `count` that is not an integer, is a caller error the client must read, not a protocol error.
fn refused(arguments: &JsonObject) -> Option<String> {
    match arguments.get("text") {
        Some(Value::String(_)) => {}
        Some(_) => return Some("`text` must be a string".to_owned()),
        None => return Some("`text` is required".to_owned()),
    }
    if arguments.contains_key("count")
        && !arguments["count"].is_i64()
        && !arguments["count"].is_u64()
    {
        return Some("`count` must be an integer".to_owned());
    }
    None
}

fn args_json(arguments: &JsonObject) -> String {
    let value = Value::Object(arguments.clone());
    serde_json::to_string(&value).expect("a JSON object serialises")
}

impl ServerHandler for Echo {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("sangkar-spike-mcp", "0.0.0"))
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(vec![Tool::new(
            TOOL_NAME,
            TOOL_DESCRIPTION,
            Self::schema(),
        )]))
    }

    /// Two outcomes, both of them JSON-RPC *results*: a good call answers `isError:false` with the
    /// arguments as text; an unknown tool or a bad argument answers `isError:true` with a readable
    /// message. The protocol error (`Err(McpError)`) path is never taken on purpose — that is the
    /// distinction question (c) is about.
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        if request.name != TOOL_NAME {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "no tool named \"{}\" in this surface; this spike registers: {TOOL_NAME}",
                request.name
            ))])
            .into());
        }
        let arguments = request.arguments.unwrap_or_default();
        if let Some(reason) = refused(&arguments) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "echo could not answer: {reason}"
            ))])
            .into());
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(args_json(&arguments))]).into())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `serve` returns once the peer's `initialize` has been answered; `waiting` then holds the
    // process until stdin closes, which is the lifecycle a stdio MCP client expects.
    let running = Echo.serve(stdio()).await?;
    running.waiting().await?;
    Ok(())
}
