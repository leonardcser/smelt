use super::background::ProcessRegistry;
use super::{str_arg, Tool, ToolResult};
use serde_json::Value;
use std::collections::HashMap;

pub fn format_read_result(output: String, running: bool, exit_code: Option<i32>) -> ToolResult {
    let status = if running {
        "running".to_string()
    } else {
        format!("exited (code {})", exit_code.unwrap_or(-1))
    };
    let content = if output.is_empty() {
        format!("[{status}]")
    } else {
        format!("{output}\n[{status}]")
    };
    ToolResult {
        content,
        is_error: false,
    }
}

pub struct ReadProcessOutputTool {
    pub registry: ProcessRegistry,
}

impl Tool for ReadProcessOutputTool {
    fn name(&self) -> &str {
        "read_process_output"
    }

    fn description(&self) -> &str {
        "Read output from a background process. Blocks until the process finishes by default. Set block=false for a non-blocking check of current output."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {"type": "string", "description": "Process ID (e.g. proc_1)"},
                "block": {"type": "boolean", "description": "Wait for process to finish (default: true). Set to false for a non-blocking check."},
                "timeout_ms": {"type": "integer", "description": "Max wait time in ms when blocking (default: 30000)"}
            },
            "required": ["id"]
        })
    }

    fn execute(&self, args: &HashMap<String, Value>) -> ToolResult {
        let id = str_arg(args, "id");
        match self.registry.read(&id) {
            Ok((output, running, exit_code)) => format_read_result(output, running, exit_code),
            Err(e) => ToolResult {
                content: e,
                is_error: true,
            },
        }
    }
}

pub struct StopProcessTool {
    pub registry: ProcessRegistry,
}

impl Tool for StopProcessTool {
    fn name(&self) -> &str {
        "stop_process"
    }

    fn description(&self) -> &str {
        "Stop a running background process and return its accumulated output."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {"type": "string", "description": "Process ID (e.g. proc_1)"}
            },
            "required": ["id"]
        })
    }

    fn execute(&self, args: &HashMap<String, Value>) -> ToolResult {
        let id = str_arg(args, "id");
        match self.registry.stop(&id) {
            Ok(output) => ToolResult {
                content: if output.is_empty() {
                    "process stopped (no output)".into()
                } else {
                    format!("process stopped\n{output}")
                },
                is_error: false,
            },
            Err(e) => ToolResult {
                content: e,
                is_error: true,
            },
        }
    }
}
