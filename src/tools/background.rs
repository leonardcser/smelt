use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;
use tokio::sync::mpsc;

struct Process {
    lines: Vec<String>,
    read_cursor: usize,
    finished: bool,
    exit_code: Option<i32>,
    command: String,
    started_at: Instant,
    /// Sends SIGKILL to the child process.
    kill_tx: Option<mpsc::Sender<()>>,
}

/// Info about a background process, returned by `list()`.
pub struct ProcessInfo {
    pub id: String,
    pub command: String,
    pub running: bool,
    pub exit_code: Option<i32>,
    pub elapsed: std::time::Duration,
}

/// Shared registry of background processes.
#[derive(Clone)]
pub struct ProcessRegistry(Arc<Mutex<HashMap<String, Process>>>);

impl ProcessRegistry {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(HashMap::new())))
    }

    /// Spawn a background process. Output is accumulated internally; a
    /// background tokio task reads stdout/stderr and marks the process
    /// finished when it exits.
    pub fn spawn(
        &self,
        id: String,
        command: &str,
        mut child: Child,
        done_tx: mpsc::UnboundedSender<(String, Option<i32>)>,
    ) {
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        let (kill_tx, mut kill_rx) = mpsc::channel::<()>(1);

        {
            let mut map = self.0.lock().unwrap();
            map.insert(
                id.clone(),
                Process {
                    lines: Vec::new(),
                    read_cursor: 0,
                    finished: false,
                    exit_code: None,
                    command: command.to_string(),
                    started_at: Instant::now(),
                    kill_tx: Some(kill_tx),
                },
            );
        }

        let registry = self.0.clone();
        let id2 = id.clone();
        tokio::spawn(async move {
            let mut stdout_reader = BufReader::new(stdout).lines();
            let mut stderr_reader = BufReader::new(stderr).lines();
            let mut stdout_done = false;
            let mut stderr_done = false;

            loop {
                if stdout_done && stderr_done {
                    break;
                }
                tokio::select! {
                    line = stdout_reader.next_line(), if !stdout_done => {
                        match line {
                            Ok(Some(line)) => {
                                let mut map = registry.lock().unwrap();
                                if let Some(p) = map.get_mut(&id2) {
                                    p.lines.push(line);
                                }
                            }
                            _ => stdout_done = true,
                        }
                    }
                    line = stderr_reader.next_line(), if !stderr_done => {
                        match line {
                            Ok(Some(line)) => {
                                let mut map = registry.lock().unwrap();
                                if let Some(p) = map.get_mut(&id2) {
                                    p.lines.push(line);
                                }
                            }
                            _ => stderr_done = true,
                        }
                    }
                    _ = kill_rx.recv() => {
                        let _ = child.kill().await;
                        break;
                    }
                }
            }

            let status = child.wait().await;
            let code = status.ok().and_then(|s| s.code());
            {
                let mut map = registry.lock().unwrap();
                if let Some(p) = map.get_mut(&id2) {
                    p.finished = true;
                    p.exit_code = code;
                    p.kill_tx = None;
                }
            }
            let _ = done_tx.send((id2, code));
        });
    }

    /// Read new output since the last read. Returns (new_lines, running, exit_code).
    pub fn read(&self, id: &str) -> Result<(String, bool, Option<i32>), String> {
        let mut map = self.0.lock().unwrap();
        let p = map.get_mut(id).ok_or_else(|| format!("no process with id '{id}'"))?;
        let new_lines = &p.lines[p.read_cursor..];
        let output = new_lines.join("\n");
        p.read_cursor = p.lines.len();
        Ok((output, !p.finished, p.exit_code))
    }

    /// Stop a background process. Returns its final accumulated output.
    pub fn stop(&self, id: &str) -> Result<String, String> {
        let kill_tx = {
            let mut map = self.0.lock().unwrap();
            let p = map.get_mut(id).ok_or_else(|| format!("no process with id '{id}'"))?;
            p.kill_tx.take()
        };
        if let Some(tx) = kill_tx {
            let _ = tx.try_send(());
        }
        // Give the background task a moment to finish
        std::thread::sleep(std::time::Duration::from_millis(100));
        let map = self.0.lock().unwrap();
        let p = map.get(id).ok_or_else(|| format!("no process with id '{id}'"))?;
        Ok(p.lines.join("\n"))
    }

    pub fn next_id(&self) -> String {
        let map = self.0.lock().unwrap();
        let n = map.len() + 1;
        format!("proc_{n}")
    }

    /// Number of currently running processes.
    pub fn running_count(&self) -> usize {
        let map = self.0.lock().unwrap();
        map.values().filter(|p| !p.finished).count()
    }

    /// List all background processes with their status.
    pub fn list(&self) -> Vec<ProcessInfo> {
        let map = self.0.lock().unwrap();
        let mut procs: Vec<ProcessInfo> = map
            .iter()
            .map(|(id, p)| ProcessInfo {
                id: id.clone(),
                command: p.command.clone(),
                running: !p.finished,
                exit_code: p.exit_code,
                elapsed: p.started_at.elapsed(),
            })
            .collect();
        procs.sort_by(|a, b| a.id.cmp(&b.id));
        procs
    }
}
