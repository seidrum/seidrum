use anyhow::{Context, Result};
use clap::Parser;
use futures::StreamExt;
use seidrum_common::events::{EventEnvelope, PluginRegister};
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
    /// NATS server URL
    #[arg(long, env = "NATS_URL", default_value = "nats://127.0.0.1:4222")]
    nats_url: String,
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
    let client = async_nats::connect(&args.nats_url)
        .await
        .context("Failed to connect to NATS")?;

    info!(url = %args.nats_url, "Connected to NATS");

    // Register plugin
    let register = PluginRegister {
        id: PLUGIN_ID.to_string(),
        name: PLUGIN_NAME.to_string(),
        version: PLUGIN_VERSION.to_string(),
        description: "Sandboxed code execution as an LLM tool".to_string(),
        consumes: vec!["tool.code-execute.request".to_string()],
        produces: vec![],
        health_subject: format!("plugin.{}.health", PLUGIN_ID),
    };

    let register_envelope = EventEnvelope::new(
        "plugin.register",
        PLUGIN_ID,
        None,
        None,
        &register,
    )?;

    client
        .publish(
            "plugin.register",
            serde_json::to_vec(&register_envelope)?.into(),
        )
        .await
        .context("Failed to publish plugin.register")?;

    info!("Plugin registered");

    // Subscribe to tool.code-execute.request using NATS request/reply
    let mut subscriber = client
        .subscribe("tool.code-execute.request")
        .await
        .context("Failed to subscribe to tool.code-execute.request")?;

    info!("Subscribed to tool.code-execute.request, waiting for requests...");

    while let Some(msg) = subscriber.next().await {
        let reply = match msg.reply {
            Some(ref r) => r.clone(),
            None => {
                warn!("Received request without reply subject, skipping");
                continue;
            }
        };

        let request: CodeExecuteRequest = match serde_json::from_slice(&msg.payload) {
            Ok(r) => r,
            Err(err) => {
                warn!(%err, "Failed to deserialize code-execute request");
                let error_response = CodeExecuteResponse {
                    stdout: String::new(),
                    stderr: format!("Invalid request payload: {}", err),
                    exit_code: -1,
                    timed_out: false,
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

        if let Err(err) = client
            .publish(reply, serde_json::to_vec(&response)?.into())
            .await
        {
            error!(%err, "Failed to publish code execution reply");
        }
    }

    Ok(())
}

/// Execute code in a subprocess with timeout and optional network isolation.
async fn execute_code(request: &CodeExecuteRequest) -> CodeExecuteResponse {
    let timeout_secs = request.timeout_seconds.min(MAX_TIMEOUT_SECONDS);

    // Determine command and args based on language
    let (program, args, use_stdin) = match request.language.as_str() {
        "python" => ("python3", vec!["-c".to_string(), request.code.clone()], false),
        "bash" => ("bash", vec!["-c".to_string(), request.code.clone()], false),
        "javascript" => ("node", vec!["-e".to_string(), request.code.clone()], false),
        other => {
            return CodeExecuteResponse {
                stdout: String::new(),
                stderr: format!("Unsupported language: {}. Supported: python, bash, javascript", other),
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
        && result.stderr.contains("unshare")
        && result.stderr.contains("ermission")
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
