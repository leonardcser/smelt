use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::AsyncReadExt;

struct Mock {
    url: String,
    requests: Arc<AtomicUsize>,
    release: Arc<tokio::sync::Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn mock(body: &'static str, delayed: bool) -> Mock {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/api.json", listener.local_addr().unwrap());
    let requests = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Notify::new());
    let counter = requests.clone();
    let gate = release.clone();
    let task = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let byte = stream.read_u8().await.unwrap();
                request.push(byte);
            }
            assert!(request.starts_with(b"GET /api.json HTTP/1.1\r\n"));
            counter.fetch_add(1, Ordering::SeqCst);
            if delayed {
                gate.notified().await;
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
    Mock {
        url,
        requests,
        release,
        task,
    }
}

fn make_service(mock: &Mock, cache_dir: Option<PathBuf>) -> Arc<Service> {
    Arc::new(Service::new(
        cache_dir,
        crate::http::client(),
        mock.url.clone(),
    ))
}

fn request(model: &str) -> Request {
    Request {
        providers: vec!["openai".into()],
        model: model.into(),
    }
}

async fn settled(service: &Service) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while service.state.lock().unwrap().refresh_scheduled {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

async fn received(mock: &Mock) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while mock.requests.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

fn cache(dir: &Path, json: &str, expires: u64) {
    let mut file = File::create(dir.join(CACHE_FILE)).unwrap();
    archive::write(json.as_bytes(), &mut file, expires).unwrap();
}

#[tokio::test]
async fn initialization_does_not_open_or_fetch_the_catalog() {
    let mock = mock("{}", false).await;
    let service = make_service(&mock, None);
    tokio::task::yield_now().await;
    assert_eq!(mock.requests.load(Ordering::SeqCst), 0);
    let state = service.state.lock().unwrap();
    assert!(state.source.is_none());
    assert!(state.requested.is_empty());
}

#[tokio::test]
async fn requests_added_during_refresh_are_resolved_without_an_allowlist() {
    let mock = mock(
        r#"{"openai":{"models":{
        "gpt-4o":{"limit":{"output":101}},
        "newly-discovered":{"limit":{"output":202}},
        "unused":{"limit":{"output":303}}
    }}}"#,
        true,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let service = make_service(&mock, Some(dir.path().into()));

    assert!(service.lookup(request("gpt-4o")).is_some());
    received(&mock).await;
    assert!(service.lookup(request("newly-discovered")).is_none());
    mock.release.notify_one();
    settled(&service).await;

    assert_eq!(
        service.lookup(request("gpt-4o")).unwrap().output_tokens,
        Some(101)
    );
    assert_eq!(
        service
            .lookup(request("newly-discovered"))
            .unwrap()
            .output_tokens,
        Some(202)
    );
    assert_eq!(service.state.lock().unwrap().requested.len(), 2);
    assert_eq!(mock.requests.load(Ordering::SeqCst), 1);
    assert!(dir.path().join(CACHE_FILE).exists());

    let restarted = make_service(&mock, Some(dir.path().into()));
    assert_eq!(
        restarted.lookup(request("unused")).unwrap().output_tokens,
        Some(303)
    );
    assert_eq!(restarted.state.lock().unwrap().requested.len(), 1);
    assert_eq!(mock.requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn fresh_cache_is_local_but_a_new_missing_model_triggers_refresh() {
    let mock = mock(
        r#"{"openai":{"models":{"new":{"limit":{"context":12345}}}}}"#,
        false,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    cache(
        dir.path(),
        r#"{"openai":{"models":{"old":{"reasoning":true}}}}"#,
        now_secs() + CACHE_TTL.as_secs(),
    );
    let service = make_service(&mock, Some(dir.path().into()));
    assert_eq!(
        service.lookup(request("old")).unwrap().supports_reasoning,
        Some(true)
    );
    assert_eq!(mock.requests.load(Ordering::SeqCst), 0);
    assert!(service.lookup(request("new")).is_none());
    settled(&service).await;
    assert_eq!(
        service.lookup(request("new")).unwrap().context_window,
        Some(12345)
    );
    assert!(service.lookup(request("old")).is_none());
    assert_eq!(mock.requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn new_misses_share_queued_refresh_and_confirmed_misses_stay_local() {
    let mock = mock(
        r#"{"openai":{"models":{"known":{"reasoning":true}}}}"#,
        false,
    )
    .await;
    let service = make_service(&mock, None);
    assert!(service.lookup(request("unknown")).is_none());
    settled(&service).await;
    for _ in 0..10 {
        assert!(service.lookup(request("unknown")).is_none());
    }
    assert_eq!(mock.requests.load(Ordering::SeqCst), 1);
    assert!(!service.state.lock().unwrap().refresh_scheduled);

    tokio::time::pause();
    assert!(service.lookup(request("new-model-a")).is_none());
    assert!(service.lookup(request("new-model-b")).is_none());
    tokio::task::yield_now().await;
    tokio::time::advance(REFRESH_INTERVAL - Duration::from_secs(1)).await;
    assert_eq!(mock.requests.load(Ordering::SeqCst), 1);
    assert!(service.state.lock().unwrap().refresh_scheduled);
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::time::resume();
    settled(&service).await;
    assert_eq!(mock.requests.load(Ordering::SeqCst), 2);
    for model in ["unknown", "new-model-a", "new-model-b"] {
        assert!(service.lookup(request(model)).is_none());
    }
    assert!(!service.state.lock().unwrap().refresh_scheduled);
}

#[tokio::test]
async fn queued_refresh_skips_models_evicted_during_cooldown() {
    let mock = mock(
        r#"{"openai":{"models":{"known":{"reasoning":true}}}}"#,
        false,
    )
    .await;
    let service = make_service(&mock, None);
    service.lookup(request("known"));
    settled(&service).await;
    tokio::time::pause();
    service.lookup(request("missing"));
    {
        let mut state = service.state.lock().unwrap();
        for index in 0..MAX_CACHED_MODELS {
            state.remember(
                request(&format!("retained-{index}")),
                Some(ModelEntry::default()),
            );
        }
        assert!(!state.requested.contains_key(&request("missing")));
    }
    tokio::task::yield_now().await;
    tokio::time::advance(REFRESH_INTERVAL).await;
    tokio::time::resume();
    settled(&service).await;
    assert_eq!(mock.requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn failed_refresh_keeps_stale_metadata_and_backs_off() {
    let mock = mock("<html>unavailable</html>", false).await;
    let dir = tempfile::tempdir().unwrap();
    cache(
        dir.path(),
        r#"{"openai":{"models":{"offline":{"limit":{"output":42}}}}}"#,
        1,
    );
    let service = make_service(&mock, Some(dir.path().into()));
    assert_eq!(
        service.lookup(request("offline")).unwrap().output_tokens,
        Some(42)
    );
    settled(&service).await;
    tokio::time::pause();
    for _ in 0..10 {
        assert_eq!(
            service.lookup(request("offline")).unwrap().output_tokens,
            Some(42)
        );
    }
    assert_eq!(mock.requests.load(Ordering::SeqCst), 1);
    assert!(service.state.lock().unwrap().refresh_after.is_some());
    tokio::task::yield_now().await;
    tokio::time::advance(REFRESH_INTERVAL).await;
    tokio::time::resume();
    settled(&service).await;
    assert_eq!(mock.requests.load(Ordering::SeqCst), 2);
    let disk = load_local(Some(dir.path())).unwrap();
    assert_eq!(disk.expires(), 1);
    assert!(disk
        .lookup(&["openai".into()], "offline")
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn concurrent_lookups_share_one_load_and_download() {
    let mock = mock(
        r#"{"openai":{"models":{"gpt-4o":{"reasoning":false}}}}"#,
        false,
    )
    .await;
    let service = make_service(&mock, None);
    let threads = (0..16)
        .map(|_| {
            let service = service.clone();
            std::thread::spawn(move || service.lookup(request("gpt-4o")))
        })
        .collect::<Vec<_>>();
    for thread in threads {
        assert!(thread.join().unwrap().is_some());
    }
    settled(&service).await;
    assert_eq!(service.state.lock().unwrap().requested.len(), 1);
    assert_eq!(mock.requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unwritable_cache_does_not_prevent_in_memory_refresh() {
    let mock = mock(r#"{"openai":{"models":{"new":{"reasoning":true}}}}"#, false).await;
    let file = tempfile::NamedTempFile::new().unwrap();
    let service = make_service(&mock, Some(file.path().into()));
    assert!(service.lookup(request("new")).is_none());
    settled(&service).await;
    assert_eq!(
        service.lookup(request("new")).unwrap().supports_reasoning,
        Some(true)
    );
}

#[tokio::test]
async fn newly_selected_provider_loads_from_refreshed_disk_source() {
    let mock = mock(
        r#"{
        "openai":{"models":{"gpt-4o":{"reasoning":false}}},
        "new-provider":{"models":{"new-model":{"limit":{"context":777}}}}
    }"#,
        false,
    )
    .await;
    let service = make_service(&mock, None);
    assert!(service.lookup(request("gpt-4o")).is_some());
    settled(&service).await;
    assert_eq!(service.state.lock().unwrap().requested.len(), 1);
    let entry = service
        .lookup(Request {
            providers: vec!["new-provider".into()],
            model: "new-model".into(),
        })
        .unwrap();
    assert_eq!(entry.context_window, Some(777));
    assert_eq!(service.state.lock().unwrap().requested.len(), 2);
    assert_eq!(mock.requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn expired_cache_refreshes_previously_memoized_models() {
    let mock = mock(
        r#"{"openai":{"models":{"old":{"limit":{"output":2}}}}}"#,
        false,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    cache(
        dir.path(),
        r#"{"openai":{"models":{"old":{"limit":{"output":1}}}}}"#,
        now_secs() + CACHE_TTL.as_secs(),
    );
    let service = make_service(&mock, Some(dir.path().into()));
    assert_eq!(
        service.lookup(request("old")).unwrap().output_tokens,
        Some(1)
    );
    assert_eq!(mock.requests.load(Ordering::SeqCst), 0);
    cache(
        dir.path(),
        r#"{"openai":{"models":{"old":{"limit":{"output":1}}}}}"#,
        1,
    );
    service.state.lock().unwrap().source = Some(Arc::new(load_local(Some(dir.path())).unwrap()));
    assert_eq!(
        service.lookup(request("old")).unwrap().output_tokens,
        Some(1)
    );
    settled(&service).await;
    assert_eq!(
        service.lookup(request("old")).unwrap().output_tokens,
        Some(2)
    );
    assert_eq!(mock.requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn damaged_provider_block_falls_back_and_refreshes() {
    let mock = mock(
        r#"{"openai":{"models":{"gpt-4o":{"limit":{"output":99}}}}}"#,
        true,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    cache(
        dir.path(),
        r#"{"openai":{"models":{"gpt-4o":{"limit":{"output":42}}}}}"#,
        now_secs() + CACHE_TTL.as_secs(),
    );
    let path = dir.path().join(CACHE_FILE);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[0] ^= 0xff;
    std::fs::write(path, bytes).unwrap();
    let service = make_service(&mock, Some(dir.path().into()));
    let entry = service.lookup(request("gpt-4o")).unwrap();
    assert_ne!(entry.output_tokens, Some(42));
    assert_eq!(
        service
            .state
            .lock()
            .unwrap()
            .source
            .as_ref()
            .unwrap()
            .expires(),
        0
    );
    received(&mock).await;
    mock.release.notify_one();
    settled(&service).await;
    assert_eq!(
        service.lookup(request("gpt-4o")).unwrap().output_tokens,
        Some(99)
    );
}

#[test]
fn memo_is_bounded_and_evicts_least_recently_used_hits_and_misses() {
    let mut state = State {
        clock: u64::MAX - MAX_CACHED_MODELS as u64 / 2,
        ..State::default()
    };
    for index in 0..MAX_CACHED_MODELS {
        let entry = (index % 2 == 0).then_some(ModelEntry::default());
        state.remember(request(&format!("model-{index}")), entry);
    }
    assert!(state.cached(&request("model-0")).flatten().is_some());
    state.remember(request("new-model"), None);
    assert_eq!(state.requested.len(), MAX_CACHED_MODELS);
    assert!(state.requested.contains_key(&request("model-0")));
    assert!(!state.requested.contains_key(&request("model-1")));
    for index in 0..MAX_CACHED_MODELS * 2 {
        state.remember(request(&format!("new-{index}")), None);
        assert_eq!(state.requested.len(), MAX_CACHED_MODELS);
    }
}

#[test]
fn refresh_invalidates_retained_values_without_expanding_the_memo() {
    let service = Service::new(None, crate::http::client(), "unused".into());
    {
        let mut state = service.state.lock().unwrap();
        for index in 0..MAX_CACHED_MODELS * 2 {
            state.remember(request(&format!("old-{index}")), None);
        }
    }
    let archive = Archive::open(Cursor::new(
        write_snapshot(r#"{"openai":{"models":{"new":{"reasoning":true}}}}"#).unwrap(),
    ))
    .unwrap();
    service.install(archive);
    let state = service.state.lock().unwrap();
    assert_eq!(state.requested.len(), MAX_CACHED_MODELS);
    assert!(!state.requested.contains_key(&request("old-0")));
    assert!(state
        .requested
        .values()
        .all(|value| value.confirmed && value.generation != state.generation));
}

#[test]
fn blocked_cold_reader_does_not_block_hits_or_publish_obsolete_values() {
    use std::sync::{atomic::AtomicBool, mpsc};

    struct GatedReader {
        bytes: Cursor<Vec<u8>>,
        armed: Arc<AtomicBool>,
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }

    impl io::Read for GatedReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.armed.swap(false, Ordering::SeqCst) {
                self.entered.send(()).unwrap();
                self.release.recv().unwrap();
            }
            io::Read::read(&mut self.bytes, buffer)
        }
    }

    impl io::Seek for GatedReader {
        fn seek(&mut self, from: io::SeekFrom) -> io::Result<u64> {
            self.bytes.seek(from)
        }
    }

    let old = write_snapshot(
        r#"{"openai":{"models":{
        "hot":{"limit":{"output":1}},"cold":{"limit":{"output":1}}
    }}}"#,
    )
    .unwrap();
    let new = write_snapshot(
        r#"{"openai":{"models":{
        "hot":{"limit":{"output":2}},"cold":{"limit":{"output":2}}
    }}}"#,
    )
    .unwrap();
    let armed = Arc::new(AtomicBool::new(false));
    let (entered, waiting) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let reader = GatedReader {
        bytes: Cursor::new(old),
        armed: armed.clone(),
        entered,
        release: released,
    };
    let service = Arc::new(Service::new(None, crate::http::client(), "unused".into()));
    service.install(Archive::open(reader).unwrap());
    assert_eq!(
        service.lookup(request("hot")).unwrap().output_tokens,
        Some(1)
    );
    armed.store(true, Ordering::SeqCst);
    let cold_service = service.clone();
    let cold = std::thread::spawn(move || cold_service.lookup(request("cold")));
    waiting.recv_timeout(Duration::from_secs(5)).unwrap();

    let (sent, received) = mpsc::channel();
    let hot_service = service.clone();
    let hot = std::thread::spawn(move || sent.send(hot_service.lookup(request("hot"))).unwrap());
    let hit = received.recv_timeout(Duration::from_secs(5));
    let (installed, completion) = mpsc::channel();
    let installer_service = service.clone();
    let installer = std::thread::spawn(move || {
        installer_service.install(Archive::open(Cursor::new(new)).unwrap());
        installed.send(()).unwrap();
    });
    let swapped = completion.recv_timeout(Duration::from_secs(5));
    // Release the reader before asserting, including on a locking regression.
    release.send(()).unwrap();
    hot.join().unwrap();
    installer.join().unwrap();
    let cold = cold.join().unwrap().unwrap();
    assert!(swapped.is_ok(), "refresh waited for the obsolete reader");
    assert_eq!(hit.unwrap().unwrap().output_tokens, Some(1));
    assert_eq!(cold.output_tokens, Some(2));
    assert_eq!(
        service.lookup(request("hot")).unwrap().output_tokens,
        Some(2)
    );
}

#[tokio::test]
async fn refresh_guard_recovers_after_cancellation() {
    let mock = mock("{}", false).await;
    let service = make_service(&mock, None);
    service.state.lock().unwrap().refresh_scheduled = true;
    drop(RefreshGuard {
        service: service.clone(),
        attempted: true,
    });
    let state = service.state.lock().unwrap();
    assert!(!state.refresh_scheduled);
    assert!(state.refresh_after.is_some());
}
