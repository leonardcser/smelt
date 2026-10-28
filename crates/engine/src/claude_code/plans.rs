use serde_json::Value;
use std::{
    collections::HashMap,
    future::Future,
    io::Read,
    path::{Path, PathBuf},
};

pub fn directory(sessions_root: &Path, session_id: &str) -> Result<PathBuf, String> {
    if session_id.is_empty()
        || !session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("Invalid session ID for Claude Code plan artifacts".into());
    }
    Ok(
        smelt_store::SessionStoreLayout::from_sessions_root(sessions_root)
            .session_artifact_dir(session_id)
            .join("claude-code-plans"),
    )
}

pub(super) struct Plans {
    directory: PathBuf,
    active: Option<PathBuf>,
    pending_writes: HashMap<String, PathBuf>,
}

impl Plans {
    pub(super) fn new(directory: PathBuf, active: Option<&Path>) -> Self {
        let active = active
            .filter(|path| relative_plan(path))
            .map(|path| directory.join(path));
        Self {
            directory,
            active,
            pending_writes: HashMap::new(),
        }
    }

    pub(super) fn active(&self) -> Option<&Path> {
        self.active
            .as_deref()
            .and_then(|path| path.strip_prefix(&self.directory).ok())
    }

    pub(super) fn register_write(
        &mut self,
        call_id: &str,
        name: &str,
        args: &HashMap<String, Value>,
    ) {
        if name != "Write" {
            return;
        }
        if let Some(path) = args
            .get("file_path")
            .and_then(Value::as_str)
            .map(PathBuf::from)
        {
            if path.strip_prefix(&self.directory).is_ok_and(relative_plan) {
                self.pending_writes.insert(call_id.into(), path);
            }
        }
    }

    pub(super) fn frame(&mut self, frame: &Value) {
        if frame["type"] != "user" || !frame["parent_tool_use_id"].is_null() {
            return;
        }
        let Some(blocks) = frame["message"]["content"].as_array() else {
            return;
        };
        for block in blocks {
            if block["type"] != "tool_result" {
                continue;
            }
            if let Some(path) = block["tool_use_id"]
                .as_str()
                .and_then(|id| self.pending_writes.remove(id))
            {
                if !block["is_error"].as_bool().unwrap_or(false) {
                    self.active = Some(path);
                }
            }
        }
    }

    pub(super) fn input(
        &self,
        input: Value,
    ) -> impl Future<Output = Result<Value, String>> + Send + 'static {
        let directory = self.directory.clone();
        let active = self.active.clone();
        async move {
            if !input.is_object() {
                return Err("Claude Code plan input must be an object".into());
            }
            if input["plan"]
                .as_str()
                .is_some_and(|plan| !plan.trim().is_empty())
            {
                return Ok(input);
            }
            let path = active.ok_or("Claude Code did not supply plan text; write the plan in the configured plansDirectory before requesting approval")?;
            let plan = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                tokio::task::spawn_blocking(move || read_plan(&directory, &path)),
            )
            .await
            .map_err(|_| "Reading the native plan timed out".to_string())?
            .map_err(|error| format!("Cannot read native plan: {error}"))??;
            let mut input = input;
            input["plan"] = Value::String(plan);
            Ok(input)
        }
    }
}

fn relative_plan(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path.extension().is_some_and(|extension| extension == "md")
        && path
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

fn read_plan(directory: &Path, path: &Path) -> Result<String, String> {
    let directory = directory
        .canonicalize()
        .map_err(|error| format!("Cannot resolve native plan directory: {error}"))?;
    let path = path
        .canonicalize()
        .map_err(|error| format!("Cannot resolve native plan: {error}"))?;
    if !path.starts_with(&directory) {
        return Err("Native plan path escapes the session artifact directory".into());
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .map_err(|error| format!("Cannot open native plan: {error}"))?;
    if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return Err("Native plan is not a regular file".into());
    }
    let mut plan = String::new();
    file.take(1024 * 1024 + 1)
        .read_to_string(&mut plan)
        .map_err(|error| format!("Cannot read native plan: {error}"))?;
    if plan.len() > 1024 * 1024 {
        return Err("Native plan exceeds 1 MiB".into());
    }
    if plan.trim().is_empty() {
        return Err("Native plan is empty".into());
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn complete(plans: &mut Plans, id: &str, is_error: bool) {
        plans.frame(&json!({"type":"user", "message":{"content":[{"type":"tool_result", "tool_use_id":id, "is_error":is_error}]}}));
    }

    #[tokio::test]
    async fn active_plan_is_an_acknowledged_write_and_survives_resume() {
        let root = tempfile::tempdir().unwrap();
        let dir = directory(root.path(), "session").unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("plan.md");
        std::fs::write(&path, "# Native plan").unwrap();
        let mut plans = Plans::new(dir.clone(), None);
        let args = HashMap::from([("file_path".into(), json!(path))]);
        plans.register_write("write", "Write", &args);
        assert!(plans.input(json!({})).await.is_err());
        complete(&mut plans, "write", false);
        assert_eq!(
            plans.input(json!({})).await.unwrap()["plan"],
            "# Native plan"
        );
        let notes = dir.join("notes.md");
        std::fs::write(&notes, "# Unrelated notes").unwrap();
        let args = HashMap::from([("file_path".into(), json!(notes))]);
        plans.register_write("read", "Read", &args);
        complete(&mut plans, "read", false);
        plans.register_write("failed", "Write", &args);
        complete(&mut plans, "failed", true);
        let resumed = Plans::new(dir, plans.active());
        assert_eq!(
            resumed.input(json!({})).await.unwrap()["plan"],
            "# Native plan"
        );
        std::fs::write(&path, "# Revised plan").unwrap();
        assert_eq!(
            resumed.input(json!({})).await.unwrap()["plan"],
            "# Revised plan"
        );
        assert!(plans.input(json!([])).await.is_err());
        std::fs::write(&path, "x".repeat(1024 * 1024 + 1)).unwrap();
        assert!(plans
            .input(json!({}))
            .await
            .unwrap_err()
            .contains("exceeds 1 MiB"));
    }

    #[tokio::test]
    async fn saved_plan_references_cannot_escape_the_artifact_directory() {
        let root = tempfile::tempdir().unwrap();
        let dir = directory(root.path(), "session").unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let outside = dir.parent().unwrap().join("outside.md");
        std::fs::write(&outside, "unrelated").unwrap();
        let plans = Plans::new(dir.clone(), Some(Path::new("../outside.md")));
        assert!(plans.input(json!({})).await.is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside, dir.join("link.md")).unwrap();
            let plans = Plans::new(dir, Some(Path::new("link.md")));
            assert!(plans
                .input(json!({}))
                .await
                .unwrap_err()
                .contains("escapes"));
        }
        assert!(directory(root.path(), "../escape").is_err());
    }
}
