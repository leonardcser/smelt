//! Subprocess + wiremock harness for integration scenarios.

use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

pub struct Harness {
    pub mock: MockServer,
    pub config_dir: TempDir,
}

impl Harness {
    /// Stand up a wiremock server and a tempdir that will become
    /// `XDG_CONFIG_HOME`. Caller mounts response stubs on
    /// `harness.mock` before driving.
    pub async fn new() -> Self {
        let mock = MockServer::start().await;
        let config_dir = tempfile::tempdir().expect("tempdir");
        Self { mock, config_dir }
    }

    /// Write a `config.yaml` that routes all provider traffic through
    /// the wiremock server. `provider_type` is one of `anthropic` /
    /// `openai` / `openai-compatible` / etc.
    pub fn write_config(&self, provider_type: &str, model: &str) {
        let smelt_dir = self.smelt_dir();
        std::fs::create_dir_all(&smelt_dir).expect("mkdir");
        let yaml = format!(
            "providers:\n  - name: test\n    type: {provider_type}\n    api_base: {api_base}\n    api_key_env: SMELT_TEST_API_KEY\n    models:\n      - {model}\n",
            api_base = self.mock.uri(),
        );
        std::fs::write(smelt_dir.join("config.yaml"), yaml).expect("write config");
    }

    /// Write `init.lua` to the tempdir. Pass empty string for no plugin
    /// configuration.
    pub fn write_init_lua(&self, src: &str) {
        let smelt_dir = self.smelt_dir();
        std::fs::create_dir_all(&smelt_dir).expect("mkdir");
        std::fs::write(smelt_dir.join("init.lua"), src).expect("write init.lua");
    }

    /// Mount a `POST /messages` stub returning a canned Anthropic SSE
    /// stream. Each entry is one SSE event (the JSON object that goes
    /// after `data: `). The stub is registered for the lifetime of the
    /// `Harness`.
    pub async fn mount_anthropic_sse(&self, events: &[Value]) {
        let mut body = String::new();
        for ev in events {
            body.push_str("data: ");
            body.push_str(&serde_json::to_string(ev).expect("serialize sse event"));
            body.push_str("\n\n");
        }
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&self.mock)
            .await;
    }

    /// Mount a `POST /messages` stub returning an HTTP error with a
    /// JSON error body. Useful for pinning the error path (401, 429,
    /// 500, etc.).
    pub async fn mount_http_error(&self, status: u16, body: Value) {
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(
                ResponseTemplate::new(status)
                    .insert_header("content-type", "application/json")
                    .set_body_json(body),
            )
            .mount(&self.mock)
            .await;
    }

    /// Run `smelt --headless --format=json -p <message>` and return the
    /// parsed JSONL events from stdout. Stderr is discarded. Events are
    /// returned as `serde_json::Value` so snapshots own the structural
    /// shape.
    pub fn run(&self, message: &str, model_ref: &str) -> RunOutput {
        let bin = env!("CARGO_BIN_EXE_smelt");
        let out = Command::new(bin)
            .args([
                "--headless",
                "--format",
                "json",
                "--no-tool-calling",
                "-m",
                model_ref,
            ])
            .arg(message)
            .env("XDG_CONFIG_HOME", self.config_dir.path())
            .env("SMELT_TEST_API_KEY", "stub-key")
            .env("NO_COLOR", "1")
            .output()
            .expect("smelt failed to launch");
        RunOutput {
            events: parse_jsonl(&out.stdout),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            status: out.status.code().unwrap_or(-1),
        }
    }

    fn smelt_dir(&self) -> PathBuf {
        self.config_dir.path().join("smelt")
    }
}

#[allow(dead_code)]
pub struct RunOutput {
    pub events: Vec<Value>,
    pub stderr: String,
    pub status: i32,
}

fn parse_jsonl(bytes: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect()
}
