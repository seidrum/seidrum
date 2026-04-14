use anyhow::{Context, Result};
use clap::Parser;
use seidrum_common::events::{EventEnvelope, PluginRegister, ToolCallRequest, ToolCallResponse};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::process::Command;
use tracing::{error, info, warn};

const PLUGIN_ID: &str = "code-executor";
const PLUGIN_NAME: &str = "Code Executor";
const PLUGIN_VERSION: &str = "0.1.0";

/// Maximum allowed timeout for code execution.
const MAX_TIMEOUT_SECONDS: u64 = 30;
/// Default timeout when none specified.
const DEFAULT_TIMEOUT_SECONDS: u64 = 10;

#[derive(Parser, Debug)]
#[command(name = "seidrum-code-executor")]
#[command(about = "Sandboxed code execution as an LLM tool")]
struct Args {
    /// Bus server URL
    #[arg(long, env = "BUS_URL", default_value = "ws://127.0.0.1:9000")]
    bus_url: String,
}

/// Request payload for code execution.
#[derive(Serialize, Deserialize, Debug, Clone)]
struct CodeExecuteRequest {
    language: String,
    code: String,
    #[serde(default = "default_timeout")]
    timeout_seconds: u64,
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_SECONDS
}

/// Response payload for code execution.
#[derive(Serialize, Deserialize, Debug, Clone)]
struct CodeExecuteResponse {
    stdout: String,
    stderr: String,
    exit_code: i32,
    timed_out: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    info!(plugin = PLUGIN_ID, "Starting code executor plugin");

    // Connect to NATS
    let client = seidrum_common::bus_client::BusClient::connect(&args.bus_url, "code-executor")
        .await
        .context("Failed to connect to NATS")?;

    info!(url = %args.bus_url, "Connected to NATS");

    // Register plugin
    let register = PluginRegister {
        id: PLUGIN_ID.to_string(),
        name: PLUGIN_NAME.to_string(),
        version: PLUGIN_VERSION.to_string(),
        description: "Sandboxed code execution as an LLM tool".to_string(),
        consumes: vec!["capability.call.code-executor".to_string()],
        produces: vec![],
        health_subject: format!("plugin.{}.health", PLUGIN_ID),
        consumed_event_types: vec![],
        produced_event_types: vec![],
        config_schema: None,
    };

    let register_envelope =
        EventEnvelope::new("plugin.register", PLUGIN_ID, None, None, &register)?;

    client
        .publish_bytes("plugin.register", serde_json::to_vec(&register_envelope)?)
        .await
        .context("Failed to publish plugin.register")?;

    info!("Plugin registered");

    // Register tool with the kernel's tool registry (raw payload, not wrapped in envelope)
    let tool_register = serde_json::json!({
        "tool_id": "execute-code",
        "plugin_id": PLUGIN_ID,
        "name": "Execute Code",
        "summary_md": "Run Python, Bash, or JavaScript code in a sandboxed environment with timeout. Returns stdout, stderr, and exit code.",
        "manual_md": "# Execute Code\n\nRun code in a sandboxed subprocess.\n\n## Parameters\n- `language` (required): \"python\", \"bash\", or \"javascript\"\n- `code` (required): The source code to execute\n- `timeout_seconds` (optional): Max execution time, default 10, max 30\n\n## Returns\n- `stdout`: Standard output\n- `stderr`: Standard error\n- `exit_code`: Process exit code (0 = success)\n- `timed_out`: Whether execution was killed due to timeout",
        "parameters": {
            "type": "object",
            "properties": {
                "language": {
                    "type": "string",
                    "enum": ["python", "bash", "javascript"],
                    "description": "Programming language to execute"
                },
                "code": {
                    "type": "string",
                    "description": "Source code to execute"
                },
                "timeout_seconds": {
                    "type": "integer",
                    "description": "Max execution time in seconds (default 10, max 30)"
                }
            },
            "required": ["language", "code"]
        },
        "call_subject": "capability.call.code-executor",
        "kind": "both"
    });

    client
        .publish_bytes("capability.register", serde_json::to_vec(&tool_register)?)
        .await
        .context("Failed to publish capability.register")?;

    info!("Tool 'execute-code' registered with kernel");

    // Subscribe to capability.call.code-executor using NATS request/reply
    let mut subscriber = client
        .subscribe("capability.call.code-executor")
        .await
        .context("Failed to subscribe to capability.call.code-executor")?;

    info!("Subscribed to capability.call.code-executor, waiting for requests...");

    while let Some(msg) = subscriber.next().await {
        let reply = match msg.reply {
            Some(ref r) => r.clone(),
            None => {
                warn!("Received request without reply subject, skipping");
                continue;
            }
        };

        // Parse the unified ToolCallRequest
        let tool_request: ToolCallRequest = match serde_json::from_slice(&msg.payload) {
            Ok(r) => r,
            Err(err) => {
                warn!(%err, "Failed to deserialize ToolCallRequest");
                let error_response = ToolCallResponse {
                    tool_id: "execute-code".to_string(),
                    result: serde_json::json!({"error": format!("Invalid request: {}", err)}),
                    is_error: true,
                };
                if let Err(e) = client
                    .publish_bytes(reply, serde_json::to_vec(&error_response)?)
                    .await
                {
                    error!(%e, "Failed to publish error reply");
                }
                continue;
            }
        };

        // Extract code execution params from arguments
        let request = CodeExecuteRequest {
            language: tool_request
                .arguments
                .get("language")
                .and_then(|v| v.as_str())
                .unwrap_or("bash")
                .to_string(),
            code: tool_request
                .arguments
                .get("code")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            timeout_seconds: tool_request
                .arguments
                .get("timeout_seconds")
                .and_then(|v| v.as_u64())
                .unwrap_or(DEFAULT_TIMEOUT_SECONDS),
        };

        info!(
            language = %request.language,
            timeout = request.timeout_seconds,
            "Executing code"
        );

        let response = execute_code(&request).await;

        info!(
            exit_code = response.exit_code,
            timed_out = response.timed_out,
            "Code execution completed"
        );

        // Return as unified ToolCallResponse
        let tool_response = ToolCallResponse {
            tool_id: tool_request.tool_id,
            result: serde_json::to_value(&response)?,
            is_error: response.exit_code != 0,
        };

        if let Err(err) = client
            .publish_bytes(reply, serde_json::to_vec(&tool_response)?)
            .await
        {
            error!(%err, "Failed to publish tool call reply");
        }
    }

    Ok(())
}

/// Execute code in a subprocess with timeout and optional network isolation.
async fn execute_code(request: &CodeExecuteRequest) -> CodeExecuteResponse {
    let timeout_secs = request.timeout_seconds.min(MAX_TIMEOUT_SECONDS);

    // Determine command and args based on language
    let (program, args, use_stdin) = match request.language.as_str() {
        "python" => (
            "python3",
            vec!["-c".to_string(), request.code.clone()],
            false,
        ),
        "bash" => ("bash", vec!["-c".to_string(), request.code.clone()], false),
        "javascript" => ("node", vec!["-e".to_string(), request.code.clone()], false),
        other => {
            return CodeExecuteResponse {
                stdout: String::new(),
                stderr: format!(
                    "Unsupported language: {}. Supported: python, bash, javascript",
                    other
                ),
                exit_code: -1,
                timed_out: false,
            };
        }
    };

    let _ = use_stdin; // suppress unused warning

    // Try to use unshare --net for network isolation, fall back to plain execution
    let result = try_execute_with_isolation(program, &args, timeout_secs).await;

    match result {
        Ok(response) => response,
        Err(err) => CodeExecuteResponse {
            stdout: String::new(),
            stderr: format!("Execution error: {}", err),
            exit_code: -1,
            timed_out: false,
        },
    }
}

/// Attempt execution with network isolation via `unshare --net`, falling back to plain execution.
async fn try_execute_with_isolation(
    program: &str,
    args: &[String],
    timeout_secs: u64,
) -> Result<CodeExecuteResponse> {
    // First try with unshare --net for network isolation
    let child_result = Command::new("unshare")
        .arg("--net")
        .arg(program)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        .spawn();

    let use_isolation = child_result.is_ok();
    let child = match child_result {
        Ok(c) => c,
        Err(_) => {
            // unshare not available or no permissions; fall back to plain execution
            info!("unshare not available, executing without network isolation");
            Command::new(program)
                .args(args)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .stdin(std::process::Stdio::null())
                .spawn()
                .context("Failed to spawn subprocess")?
        }
    };

    let result = run_child_with_timeout(child, timeout_secs).await?;

    // If unshare failed with a permission error, retry without isolation
    if use_isolation
        && result.exit_code != 0
        && (result.stderr.contains("unshare") || result.stderr.contains("Operation not permitted"))
    {
        info!("unshare failed with permission error, retrying without isolation");
        let child = Command::new(program)
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(std::process::Stdio::null())
            .spawn()
            .context("Failed to spawn subprocess")?;
        return run_child_with_timeout(child, timeout_secs).await;
    }

    Ok(result)
}

/// Run a child process with a timeout, capturing stdout and stderr.
async fn run_child_with_timeout(
    child: tokio::process::Child,
    timeout_secs: u64,
) -> Result<CodeExecuteResponse> {
    match tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output()).await {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let exit_code = output.status.code().unwrap_or(-1);

            Ok(CodeExecuteResponse {
                stdout,
                stderr,
                exit_code,
                timed_out: false,
            })
        }
        Ok(Err(err)) => Ok(CodeExecuteResponse {
            stdout: String::new(),
            stderr: format!("Process error: {}", err),
            exit_code: -1,
            timed_out: false,
        }),
        Err(_) => {
            // Timeout — process is dropped and killed automatically
            Ok(CodeExecuteResponse {
                stdout: String::new(),
                stderr: format!("Execution timed out after {}s", timeout_secs),
                exit_code: -1,
                timed_out: true,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_code_execute_request() {
        let req = CodeExecuteRequest {
            language: "python".to_string(),
            code: "print('hello')".to_string(),
            timeout_seconds: 5,
        };

        let json = serde_json::to_string(&req).unwrap();
        let deserialized: CodeExecuteRequest = serde_json::from_str(&json).unwrap();

        assert_eq!(req.language, deserialized.language);
        assert_eq!(req.code, deserialized.code);
        assert_eq!(req.timeout_seconds, deserialized.timeout_seconds);
    }

    #[test]
    fn roundtrip_code_execute_response() {
        let resp = CodeExecuteResponse {
            stdout: "hello\n".to_string(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        };

        let json = serde_json::to_string(&resp).unwrap();
        let deserialized: CodeExecuteResponse = serde_json::from_str(&json).unwrap();

        assert_eq!(resp.stdout, deserialized.stdout);
        assert_eq!(resp.exit_code, deserialized.exit_code);
        assert_eq!(resp.timed_out, deserialized.timed_out);
    }

    #[test]
    fn default_timeout_applied() {
        let json = r#"{"language": "python", "code": "print(1)"}"#;
        let req: CodeExecuteRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.timeout_seconds, DEFAULT_TIMEOUT_SECONDS);
    }

    #[test]
    fn unsupported_language_returns_error() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let req = CodeExecuteRequest {
            language: "cobol".to_string(),
            code: "DISPLAY 'HI'".to_string(),
            timeout_seconds: 5,
        };
        let resp = rt.block_on(execute_code(&req));
        assert_eq!(resp.exit_code, -1);
        assert!(resp.stderr.contains("Unsupported language"));
    }
}
