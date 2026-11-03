//! Explicit workflow completion, scoped to the session and turn that spawned a
//! shell. Shells of an agent process that outlives turns, such as Claude Code,
//! are scoped to the session only.

use std::collections::HashMap;
use std::io;
#[cfg(unix)]
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};

use protocol::event::{CONTROL_SESSION_ENV, CONTROL_SOCKET_ENV, CONTROL_TURN_ENV};
#[cfg(unix)]
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(unix)]
const MAX_REQUEST_BYTES: u64 = 4096;

#[derive(Default)]
struct State {
    session: String,
    turn: Option<u64>,
    requested: bool,
    completed: bool,
}

#[cfg(unix)]
impl State {
    fn request(&mut self, session: &str, turn: Option<u64>) -> Result<(), String> {
        if self.session != session
            || self.turn.is_none()
            || turn.is_some_and(|turn| self.turn != Some(turn))
        {
            return Err("completion request belongs to an inactive session or turn".into());
        }
        self.requested = true;
        Ok(())
    }
}

#[derive(Default)]
pub struct SessionControl {
    state: Arc<Mutex<State>>,
    #[cfg(unix)]
    server: OnceLock<Option<Server>>,
}

impl SessionControl {
    /// Failure disables this optional channel and is reported only on the first attempt.
    pub fn initialize(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            let mut error = None;
            self.server
                .get_or_init(|| match Server::start(Arc::clone(&self.state)) {
                    Ok(server) => Some(server),
                    Err(cause) => {
                        error = Some(cause);
                        None
                    }
                });
            if let Some(error) = error {
                return Err(error);
            }
        }
        Ok(())
    }

    pub fn reset(&self) {
        *self.state.lock().unwrap() = State::default();
    }

    pub fn begin_turn(&self, session: &str, turn: u64) {
        let mut state = self.state.lock().unwrap();
        *state = State {
            session: session.into(),
            turn: Some(turn),
            ..State::default()
        };
    }

    pub fn finish_turn(&self, session: &str, turn: u64, eligible: bool) -> Option<&'static str> {
        let mut state = self.state.lock().unwrap();
        if state.session != session || state.turn != Some(turn) {
            return None;
        }
        let rejected = state.requested && !eligible;
        state.completed = eligible && state.requested;
        state.requested = false;
        state.turn = None;
        rejected.then_some(
            "session completion not applied: the turn did not succeed or still has pending work",
        )
    }

    pub fn is_completed(&self, session: &str) -> bool {
        let state = self.state.lock().unwrap();
        state.session == session && state.completed
    }

    /// Captured before spawning, so a delayed shell cannot acquire a later turn's identity.
    pub fn shell_env(&self) -> HashMap<String, String> {
        self.identity()
            .map(|(mut env, turn)| {
                env.insert(CONTROL_TURN_ENV.into(), turn.to_string());
                env
            })
            .unwrap_or_default()
    }

    /// For an agent process that outlives turns. Its shells are scoped to the
    /// session, not a turn: they complete whichever turn is active when they run.
    pub fn agent_env(&self) -> HashMap<String, String> {
        self.identity().map(|(env, _)| env).unwrap_or_default()
    }

    /// The socket and session of the active turn, and that turn.
    fn identity(&self) -> Option<(HashMap<String, String>, u64)> {
        #[cfg(unix)]
        {
            let state = self.state.lock().unwrap();
            if let (Some(turn), Some(Some(server))) = (state.turn, self.server.get()) {
                let env = HashMap::from([
                    (
                        CONTROL_SOCKET_ENV.into(),
                        server.path.to_string_lossy().into_owned(),
                    ),
                    (CONTROL_SESSION_ENV.into(), state.session.clone()),
                ]);
                return Some((env, turn));
            }
        }
        None
    }
}

#[cfg(unix)]
struct Server {
    path: std::path::PathBuf,
    task: tokio::task::JoinHandle<()>,
    _directory: tempfile::TempDir,
}

#[cfg(unix)]
impl Server {
    fn start(state: Arc<Mutex<State>>) -> io::Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let runtime = tokio::runtime::Handle::try_current().map_err(io::Error::other)?;
        let directory = tempfile::Builder::new()
            .prefix("smelt-control-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()?;
        let path = directory.path().join("control.sock");
        let listener = tokio::net::UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let task = runtime.spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(TIMEOUT, serve(stream, state)).await;
                });
            }
        });
        Ok(Self {
            path,
            task,
            _directory: directory,
        })
    }
}

#[cfg(unix)]
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(unix)]
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CompleteRequest {
    session: String,
    /// Absent for a shell of an agent process that spans turns.
    #[serde(default)]
    turn: Option<u64>,
}

#[cfg(unix)]
async fn serve(stream: tokio::net::UnixStream, state: Arc<Mutex<State>>) -> io::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    BufReader::new(read)
        .take(MAX_REQUEST_BYTES)
        .read_line(&mut line)
        .await?;
    let result = serde_json::from_str::<CompleteRequest>(&line)
        .map_err(|_| "invalid session completion request".to_string())
        .and_then(|request| {
            state
                .lock()
                .unwrap()
                .request(&request.session, request.turn)
        });
    let response = serde_json::to_vec(&result).map_err(io::Error::other)?;
    write.write_all(&response).await?;
    write.write_all(b"\n").await
}

/// The CLI runs in a separate process; no cwd lookup or status-file mutation is needed.
pub async fn complete_from_env() -> Result<(), String> {
    let required = |key| {
        std::env::var(key).map_err(|_| {
            "no current smelt session; run this command from a shell that smelt or its agent started".to_string()
        })
    };
    let socket = required(CONTROL_SOCKET_ENV)?;
    let session = required(CONTROL_SESSION_ENV)?;
    let turn = std::env::var(CONTROL_TURN_ENV)
        .ok()
        .map(|turn| {
            turn.parse()
                .map_err(|_| "invalid smelt turn identity".to_string())
        })
        .transpose()?;
    request_completion(&socket, &session, turn).await
}

/// Without `turn`, completes whichever turn of `session` is active.
pub async fn request_completion(
    socket: &str,
    session: &str,
    turn: Option<u64>,
) -> Result<(), String> {
    let request = CompleteRequest {
        session: session.into(),
        turn,
    };
    #[cfg(unix)]
    {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        tokio::time::timeout(TIMEOUT, async {
            let mut stream = tokio::net::UnixStream::connect(socket)
                .await
                .map_err(|error| format!("cannot reach current smelt session: {error}"))?;
            let mut request = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
            request.push(b'\n');
            stream
                .write_all(&request)
                .await
                .map_err(|error| error.to_string())?;
            let mut response = String::new();
            BufReader::new(stream)
                .take(MAX_REQUEST_BYTES)
                .read_line(&mut response)
                .await
                .map_err(|error| error.to_string())?;
            serde_json::from_str::<Result<(), String>>(&response)
                .map_err(|_| "invalid response from current smelt session".to_string())?
        })
        .await
        .map_err(|_| "current smelt session did not respond".to_string())?
    }
    #[cfg(not(unix))]
    {
        let _ = (socket, request);
        Err("session completion requires Unix sockets".into())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn agent_env_completes_the_active_turn_of_its_session() {
        let control = SessionControl::default();
        control.initialize().unwrap();
        control.begin_turn("session-a", 1);
        let env = control.agent_env();
        assert!(!env.contains_key(CONTROL_TURN_ENV));
        assert_eq!(env[CONTROL_SESSION_ENV], "session-a");
        let socket = &env[CONTROL_SOCKET_ENV];
        assert!(request_completion(socket, "session-b", None).await.is_err());

        control.begin_turn("session-a", 2);
        request_completion(socket, "session-a", None).await.unwrap();
        assert!(control.finish_turn("session-a", 2, true).is_none());
        assert!(control.is_completed("session-a"));

        assert!(
            request_completion(socket, "session-a", None).await.is_err(),
            "no turn is active between turns"
        );
    }

    #[tokio::test]
    async fn completion_is_scoped_to_successful_turn() {
        let control = SessionControl::default();
        control.initialize().unwrap();
        control.begin_turn("session-a", 1);
        let env = control.shell_env();
        let socket = &env[CONTROL_SOCKET_ENV];
        assert!(request_completion(socket, "session-b", Some(1))
            .await
            .is_err());
        assert!(request_completion(socket, "session-a", Some(2))
            .await
            .is_err());
        request_completion(socket, "session-a", Some(1))
            .await
            .unwrap();
        request_completion(socket, "session-a", Some(1))
            .await
            .unwrap();
        assert!(!control.is_completed("session-a"));
        assert!(control.finish_turn("session-a", 1, true).is_none());
        assert!(control.is_completed("session-a"));
        assert!(!control.is_completed("session-b"));
        assert!(request_completion(socket, "session-a", Some(1))
            .await
            .is_err());
        control.begin_turn("session-a", 2);
        assert!(!control.is_completed("session-a"));
        request_completion(socket, "session-a", Some(2))
            .await
            .unwrap();
        assert!(control.finish_turn("session-a", 2, false).is_some());
        assert!(control.finish_turn("session-a", 2, false).is_none());
        assert!(!control.is_completed("session-a"));
        control.begin_turn("session-b", 1);
        assert!(request_completion(socket, "session-a", Some(2))
            .await
            .is_err());
    }

    #[test]
    fn unavailable_control_channel_does_not_supply_shell_identity() {
        let control = SessionControl::default();
        assert!(
            control.initialize().is_err(),
            "no async runtime is available"
        );
        control.begin_turn("session", 1);
        assert!(control.shell_env().is_empty());
        assert!(
            control.initialize().is_ok(),
            "report startup failure only once"
        );
    }

    #[tokio::test]
    async fn reset_invalidates_pending_and_completed_work() {
        let control = SessionControl::default();
        control.initialize().unwrap();
        for eligible in [false, true] {
            control.begin_turn("session", 1);
            let env = control.shell_env();
            request_completion(&env[CONTROL_SOCKET_ENV], "session", Some(1))
                .await
                .unwrap();
            if eligible {
                assert!(control.finish_turn("session", 1, true).is_none());
                assert!(control.is_completed("session"));
            }
            control.reset();
            assert!(control.shell_env().is_empty());
            assert!(!control.is_completed("session"));
            assert!(
                request_completion(&env[CONTROL_SOCKET_ENV], "session", Some(1))
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn endpoint_is_private_and_removed_on_drop() {
        use std::os::unix::fs::PermissionsExt;
        let control = SessionControl::default();
        control.initialize().unwrap();
        control.begin_turn("session", 1);
        let env = control.shell_env();
        let path = std::path::Path::new(&env[CONTROL_SOCKET_ENV]);
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        drop(control);
        assert!(!path.exists());
    }
}
