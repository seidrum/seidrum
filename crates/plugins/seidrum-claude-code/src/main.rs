use anyhow::{Context, Result};
use clap::Parser;
use futures::StreamExt;
use seidrum_common::events::{EventEnvelope, PluginRegister, ToolCallRequest, ToolCallResponse};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::process::Command;
use tracing::{error, info, warn};

const PLUGIN_ID: &str = "claude-code";
const PLUGIN_NAME: &str = "Claude Code";
const PLUGIN_VERSION: &str = "0.1.0";

#[derive(Parser, Debug)]
#[command(name = "seidrum-claude-code")]
#[command(about = "Claude Code CLI as an LLM tool and Telegram command")]
struct Args {
    /// NATS server URL
    #[arg(long, env = "NATS_URL", default_value = "nats://127.0.0.1:4222")]
    nats_url: String,

    /// Default working directory for Claude Code
    #[arg(long, env = "CLAUDE_WORKING_DIR", default_value = ".")]
    working_dir: String,

    /// Comma-separated list of allowed tools
    #[arg(
        long,
        env = "CLAUDE_ALLOWED_TOOLS",
        default_value = "Read,Edit,Bash,Glob,Grep,Write"
    )]
    allowed_tools: String,

    /// Maximum agentic turns
    #[arg(long, env = "CLAUDE_MAX_TURNS", default_value = "25")]
    max_turns: u32,

    /// Maximum budget in USD (0 = unlimited)
    #[arg(long, env = "CLAUDE_MAX_BUDGET_USD", default_value = "1.0")]
    max_budget_usd: f64,

    /// Timeout in seconds for the subprocess
    #[arg(long, env = "CLAUDE_TIMEOUT_SECONDS", default_value = "300")]
    timeout_seconds: u64,

    /// Path to claude CLI binary
    #[arg(long, env = "CLAUDE_CLI_PATH", default_value = "claude")]
    claude_cli_path: String,

    /// Model to use
    #[arg(long, env = "CLAUDE_MODEL", default_value = "")]
    model: String,
}

/// Claude Code CLI JSON output (--output-format json).
#[derive(Serialize, Deserialize, Debug, Clone)]
struct ClaudeCodeOutput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    result: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<serde_json::Value>,
}

/// Response sent back via ToolCallResponse.result.
#[derive(Serialize, Deserialize, Debug, Clone)]
struct ClaudeCodeResponse {
    result: String,
    session_id: Option<String>,
    model: Option<String>,
    usage: Option<serde_json::Value>,
    exit_code: i32,
    timed_out: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    info!(plugin = PLUGIN_ID, "Starting Claude Code plugin");

    // Connect to NATS
    let client = async_nats::connect(&args.nats_url)
        .await
        .context("Failed to connect to NATS")?;

    info!(url = %args.nats_url, "Connected to NATS");

    // Register plugin
    let register = PluginRegister {
        id: PLUGIN_ID.to_string(),
        name: PLUGIN_NAME.to_string(),
        version: PLUGIN_VERSION.to_string(),
        description: "Run Claude Code CLI for agentic coding tasks".to_string(),
        consumes: vec!["capability.call.claude-code".to_string()],
        produces: vec![],
        health_subject: format!("plugin.{}.health", PLUGIN_ID),
        consumed_event_types: vec![],
        produced_event_types: vec![],
    };

    let register_envelope =
        EventEnvelope::new("plugin.register", PLUGIN_ID, None, None, &register)?;

    client
        .publish(
            "plugin.register",
            serde_json::to_vec(&register_envelope)?.into(),
        )
        .await
        .context("Failed to publish plugin.register")?;

    info!("Plugin registered");

    // Register capability
    let tool_register = serde_json::json!({
        "tool_id": "claude-code",
        "plugin_id": PLUGIN_ID,
        "name": "Claude Code",
        "summary_md": "Run Claude Code CLI to perform agentic coding tasks (edit files, run commands, analyze code, etc.)",
        "manual_md": "# Claude Code\n\nInvoke Claude Code CLI in non-interactive mode.\n\n## Parameters\n- `prompt` (required): The coding task or question\n- `working_dir` (optional): Project directory (overrides default)\n- `session_id` (optional): Resume a previous session\n- `append_system_prompt` (optional): Additional system context\n\n## Returns\n- `result`: Claude Code's text response\n- `session_id`: Session ID for resuming\n- `model`: Model used\n- `usage`: Token usage details",
        "parameters": {
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "The coding task or question"
                },
                "working_dir": {
                    "type": "string",
                    "description": "Project directory to work in (overrides default)"
                },
                "session_id": {
                    "type": "string",
                    "description": "Resume a previous Claude Code session"
                },
                "append_system_prompt": {
                    "type": "string",
                    "description": "Additional system prompt context"
                }
            },
            "required": ["prompt"],
            "metadata": {
                "command_alias": "claude"
            }
        },
        "call_subject": "capability.call.claude-code",
        "kind": "both"
    });

    client
        .publish(
            "capability.register",
            serde_json::to_vec(&tool_register)?.into(),
        )
        .await
        .context("Failed to publish capability.register")?;

    info!("Capability 'claude-code' registered");

    // Subscribe to capability calls
    let mut subscriber = client
        .subscribe("capability.call.claude-code")
        .await
        .context("Failed to subscribe to capability.call.claude-code")?;

    info!("Subscribed to capability.call.claude-code, waiting for requests...");

    while let Some(msg) = subscriber.next().await {
        let reply = match msg.reply {
            Some(ref r) => r.clone(),
            None => {
                warn!("Received request without reply subject, skipping");
                continue;
            }
        };

        // Parse ToolCallRequest
        let tool_request: ToolCallRequest = match serde_json::from_slice(&msg.payload) {
            Ok(r) => r,
            Err(err) => {
                warn!(%err, "Failed to deserialize ToolCallRequest");
                let error_response = ToolCallResponse {
                    tool_id: "claude-code".to_string(),
                    result: serde_json::json!({"error": format!("Invalid request: {}", err)}),
                    is_error: true,
                };
                if let Err(e) = client
                    .publish(reply, serde_json::to_vec(&error_response)?.into())
                    .await
                {
                    error!(%e, "Failed to publish error reply");
                }
                continue;
            }
        };

        // Extract prompt: tool calls use "prompt", Telegram commands use "args"
        let prompt = tool_request
            .arguments
            .get("prompt")
            .and_then(|v| v.as_str())
            .or_else(|| tool_request.arguments.get("args").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string();

        if prompt.is_empty() {
            let error_response = ToolCallResponse {
                tool_id: tool_request.tool_id,
                result: serde_json::json!(
                    "Please provide a prompt. Usage: /claude <task or question>"
                ),
                is_error: true,
            };
            if let Err(e) = client
                .publish(reply, serde_json::to_vec(&error_response)?.into())
                .await
            {
                error!(%e, "Failed to publish error reply");
            }
            continue;
        }

        // Extract optional parameters
        let working_dir = tool_request
            .arguments
            .get("working_dir")
            .and_then(|v| v.as_str())
            .unwrap_or(&args.working_dir)
            .to_string();

        let session_id = tool_request
            .arguments
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let append_system_prompt = tool_request
            .arguments
            .get("append_system_prompt")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        info!(
            %prompt,
            %working_dir,
            session_id = ?session_id,
            "Executing Claude Code"
        );

        let response = run_claude_code(
            &args,
            &prompt,
            &working_dir,
            session_id.as_deref(),
            append_system_prompt.as_deref(),
        )
        .await;

        info!(
            exit_code = response.exit_code,
            timed_out = response.timed_out,
            "Claude Code execution completed"
        );

        let tool_response = ToolCallResponse {
            tool_id: tool_request.tool_id,
            result: serde_json::to_value(&response)?,
            is_error: response.exit_code != 0,
        };

        if let Err(err) = client
            .publish(reply, serde_json::to_vec(&tool_response)?.into())
            .await
        {
            error!(%err, "Failed to publish tool call reply");
        }
    }

    Ok(())
}

/// Spawn Claude Code CLI as a subprocess and capture its JSON output.
async fn run_claude_code(
    config: &Args,
    prompt: &str,
    working_dir: &str,
    session_id: Option<&str>,
    append_system_prompt: Option<&str>,
) -> ClaudeCodeResponse {
    let mut cmd = Command::new(&config.claude_cli_path);

    // Non-interactive mode with prompt
    cmd.arg("-p").arg(prompt);

    // JSON output
    cmd.arg("--output-format").arg("json");

    // Allowed tools
    cmd.arg("--allowedTools").arg(&config.allowed_tools);

    // Max turns
    cmd.arg("--max-turns").arg(config.max_turns.to_string());

    // Budget limit
    if config.max_budget_usd > 0.0 {
        cmd.arg("--max-budget-usd")
            .arg(config.max_budget_usd.to_string());
    }

    // Model override
    if !config.model.is_empty() {
        cmd.arg("--model").arg(&config.model);
    }

    // Session resume
    if let Some(sid) = session_id {
        cmd.arg("--resume").arg(sid);
    }

    // Additional system prompt
    if let Some(asp) = append_system_prompt {
        cmd.arg("--append-system-prompt").arg(asp);
    }

    // Working directory
    cmd.current_dir(working_dir);

    // Pipe config
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.stdin(std::process::Stdio::null());

    // Spawn
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(err) => {
            error!(%err, "Failed to spawn claude CLI");
            return ClaudeCodeResponse {
                result: format!("Failed to start Claude Code CLI: {}", err),
                session_id: None,
                model: None,
                usage: None,
                exit_code: -1,
                timed_out: false,
            };
        }
    };

    // Wait with timeout
    match tokio::time::timeout(
        Duration::from_secs(config.timeout_seconds),
        child.wait_with_output(),
    )
    .await
    {
        Ok(Ok(output)) => {
            let exit_code = output.status.code().unwrap_or(-1);
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();

            if exit_code == 0 {
                // Parse JSON output
                match serde_json::from_str::<ClaudeCodeOutput>(&stdout) {
                    Ok(parsed) => ClaudeCodeResponse {
                        result: parsed.result,
                        session_id: parsed.session_id,
                        model: parsed.model,
                        usage: parsed.usage,
                        exit_code,
                        timed_out: false,
                    },
                    Err(_) => {
                        // Fallback: return raw output
                        ClaudeCodeResponse {
                            result: stdout,
                            session_id: None,
                            model: None,
                            usage: None,
                            exit_code,
                            timed_out: false,
                        }
                    }
                }
            } else {
                ClaudeCodeResponse {
                    result: if stderr.is_empty() { stdout } else { stderr },
                    session_id: None,
                    model: None,
                    usage: None,
                    exit_code,
                    timed_out: false,
                }
            }
        }
        Ok(Err(err)) => ClaudeCodeResponse {
            result: format!("Process error: {}", err),
            session_id: None,
            model: None,
            usage: None,
            exit_code: -1,
            timed_out: false,
        },
        Err(_) => ClaudeCodeResponse {
            result: format!("Claude Code timed out after {}s", config.timeout_seconds),
            session_id: None,
            model: None,
            usage: None,
            exit_code: -1,
            timed_out: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_claude_code_output() {
        let raw = r#"{"session_id":"s1","result":"Done","model":"claude-sonnet-4-20250514","usage":{"input_tokens":10}}"#;
        let parsed: ClaudeCodeOutput = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.result, "Done");
        assert_eq!(parsed.session_id.unwrap(), "s1");
        assert!(parsed.model.is_some());
        assert!(parsed.usage.is_some());
    }

    #[test]
    fn parse_minimal_output() {
        let raw = r#"{"result":"Hello"}"#;
        let parsed: ClaudeCodeOutput = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.result, "Hello");
        assert!(parsed.session_id.is_none());
        assert!(parsed.model.is_none());
    }

    #[test]
    fn roundtrip_claude_code_response() {
        let resp = ClaudeCodeResponse {
            result: "Fixed the bug".to_string(),
            session_id: Some("sess-1".to_string()),
            model: Some("claude-sonnet-4-20250514".to_string()),
            usage: Some(serde_json::json!({"input_tokens": 100, "output_tokens": 50})),
            exit_code: 0,
            timed_out: false,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let de: ClaudeCodeResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp.result, de.result);
        assert_eq!(resp.session_id, de.session_id);
        assert_eq!(resp.exit_code, de.exit_code);
        assert_eq!(resp.timed_out, de.timed_out);
    }

    #[test]
    fn prompt_extraction_from_tool_call() {
        // Direct tool call (from LLM router)
        let args = serde_json::json!({"prompt": "fix bug"});
        let prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .or_else(|| args.get("args").and_then(|v| v.as_str()))
            .unwrap_or("");
        assert_eq!(prompt, "fix bug");
    }

    #[test]
    fn prompt_extraction_from_telegram() {
        // Telegram command call (from /claude fix bug)
        let args = serde_json::json!({"args": "fix bug", "chat_id": "123"});
        let prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .or_else(|| args.get("args").and_then(|v| v.as_str()))
            .unwrap_or("");
        assert_eq!(prompt, "fix bug");
    }

    #[test]
    fn empty_prompt_detected() {
        let args = serde_json::json!({"args": "", "chat_id": "123"});
        let prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .or_else(|| args.get("args").and_then(|v| v.as_str()))
            .unwrap_or("");
        assert!(prompt.is_empty());
    }
}
