//! Shared types, constants, and fail-loud helpers for the MCP adapter.
//!
//! The MCP surface is a thin contract adapter over the prview core. Every
//! response carries `schema_version`, and every failure is a structured,
//! caller-visible error (`is_error: true`) with an `error_class` — never an
//! empty success. See `2026-07-01-prview-mcp-v1-design.md` sections 2, 4.

use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{Value, json};

/// Schema version stamped into every tool response.
pub const SCHEMA_VERSION: &str = "prview.mcp.v1";

/// Fail-loud error classes (design spec section 4).
pub mod error_class {
    pub const REPO_NOT_FOUND: &str = "repo_not_found";
    pub const NOT_A_GIT_REPO: &str = "not_a_git_repo";
    pub const RUN_FAILED: &str = "run_failed";
    pub const RUN_TIMEOUT: &str = "run_timeout";
    pub const RUN_NOT_FOUND: &str = "run_not_found";
    pub const ARTIFACT_MISSING: &str = "artifact_missing";
    pub const TOOL_MISSING: &str = "tool_missing";
    pub const STORAGE_LOCKED: &str = "storage_locked";
    pub const STORAGE_CORRUPT: &str = "storage_corrupt";
    pub const STALE_RUN: &str = "stale_run";
    /// Not part of the wire contract; used only by Task-1 stubs until each tool
    /// is implemented. No production path returns this.
    pub const UNIMPLEMENTED: &str = "unimplemented";
}

/// Structured error carried by internal `read`/`run` helpers so a failing
/// deep-path can be surfaced as a fail-loud `CallToolResult` at the tool
/// boundary without losing its class or auxiliary data.
#[derive(Debug, Clone)]
pub struct ToolError {
    pub class: String,
    pub message: String,
    pub extra: Value,
}

impl ToolError {
    pub fn new(class: &str, message: impl Into<String>) -> Self {
        Self {
            class: class.to_string(),
            message: message.into(),
            extra: json!({}),
        }
    }

    pub fn with_extra(class: &str, message: impl Into<String>, extra: Value) -> Self {
        Self {
            class: class.to_string(),
            message: message.into(),
            extra,
        }
    }

    /// Convert into a fail-loud `CallToolResult`.
    pub fn into_result(self) -> CallToolResult {
        tool_error(&self.class, &self.message, self.extra)
    }
}

fn stamp_schema(mut value: Value) -> Value {
    if let Value::Object(map) = &mut value {
        map.insert(
            "schema_version".to_string(),
            Value::String(SCHEMA_VERSION.to_string()),
        );
    }
    value
}

fn json_content(value: &Value) -> Vec<ContentBlock> {
    let text = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    vec![ContentBlock::text(text)]
}

/// Build a fail-loud error result: `{error_class, message, schema_version, ...extra}`.
pub fn tool_error(class: &str, message: &str, extra: Value) -> CallToolResult {
    let mut body = json!({
        "error_class": class,
        "message": message,
    });
    if let (Value::Object(dst), Value::Object(src)) = (&mut body, extra) {
        for (k, v) in src {
            dst.insert(k, v);
        }
    }
    let body = stamp_schema(body);
    CallToolResult::error(json_content(&body))
}

/// Build a success result; injects `schema_version` into the payload object.
pub fn tool_success(value: Value) -> CallToolResult {
    let body = stamp_schema(value);
    CallToolResult::success(json_content(&body))
}
