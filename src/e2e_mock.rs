//! Mock Bot API end-to-end test. The whole `gateway::run` daemon and the
//! `telegram_send` tool path run unmodified; only the HTTP base URL is
//! retargeted at a loopback stub via GRAY_TELEGRAM_TEST_API_BASE
//! (compiled in test builds only, see transport::Rest::production).
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct MockApi {
    base: String,
    state: Arc<State>,
}

struct State {
    updates: Mutex<VecDeque<Value>>,
    calls: Mutex<Vec<(String, Value)>>,
}

impl MockApi {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(State {
            updates: Mutex::new(VecDeque::new()),
            calls: Mutex::new(Vec::new()),
        });
        let st = state.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(stream) = conn else { break };
                let st = st.clone();
                std::thread::spawn(move || serve(stream, st));
            }
        });
        Self {
            base: format!("http://127.0.0.1:{port}"),
            state,
        }
    }

    fn push(&self, update: Value) {
        self.state.updates.lock().unwrap().push_back(update);
    }

    fn calls(&self, method: &str) -> Vec<Value> {
        self.state
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _)| m == method)
            .map(|(_, b)| b.clone())
            .collect()
    }

    fn wait_for(&self, method: &str, pred: impl Fn(&Value) -> bool, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if self.calls(method).iter().any(&pred) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }
}

fn read_request(stream: &mut std::net::TcpStream) -> Option<(String, String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        if buf.len() > 65536 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut parts = head.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let len = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|v| v.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    let mut body = buf[head_end..].to_vec();
    while body.len() < len {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    Some((method, path, body))
}

fn respond(stream: &mut std::net::TcpStream, code: u16, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {code} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn serve(mut stream: std::net::TcpStream, state: Arc<State>) {
    let Some((_method, path, body)) = read_request(&mut stream) else {
        return;
    };
    let params: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    if path.starts_with("/file/bot") {
        // GET /file/bot<token>/<file_path> — the file download endpoint.
        return respond(&mut stream, 200, b"hello file e2e\n");
    }
    let api_method = path
        .strip_prefix("/botTESTTOKEN/")
        .map(str::to_string)
        .unwrap_or_else(|| path.clone());
    let result = match api_method.as_str() {
        "getUpdates" => {
            let batch: Vec<Value> = {
                let mut q = state.updates.lock().unwrap();
                q.drain(..).collect()
            };
            if batch.is_empty() {
                // Short idle so the poll loop does not spin at full tilt.
                std::thread::sleep(Duration::from_millis(120));
            }
            json!(batch)
        }
        "getMe" => json!({"id": 4242, "is_bot": true, "username": "mockbot"}),
        "getFile" => json!({"file_path": "docs/readme.txt", "file_size": 14}),
        "sendMessage" => {
            state
                .calls
                .lock()
                .unwrap()
                .push(("sendMessage".to_string(), params.clone()));
            json!({"message_id": 9000 + state.calls.lock().unwrap().len() as i64})
        }
        "deleteWebhook" | "sendChatAction" => json!(true),
        _ => {
            state
                .calls
                .lock()
                .unwrap()
                .push((api_method.clone(), params.clone()));
            json!(true)
        }
    };
    if !matches!(api_method.as_str(), "sendMessage" | "getUpdates") {
        state.calls.lock().unwrap().push((api_method, params));
    }
    respond(
        &mut stream,
        200,
        json!({"ok": true, "result": result}).to_string().as_bytes(),
    );
}

fn write_executable(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mock_bot_api_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    // fake `gray -p --json`: one protocol-1 NDJSON result, exit 0.
    let fake = dir.join("fake-gray");
    write_executable(
        &fake,
        "#!/bin/sh\necho '{\"protocol\":1,\"turn_id\":\"t1\",\"session_id\":\"sess-1\",\"type\":\"result\",\"text\":\"E2E MOCK ANSWER from fake gray\"}'\n",
    );
    let ghome = dir.join("ghome");
    std::fs::create_dir(&ghome).unwrap();
    std::fs::write(ghome.join("config.json"), "{\"model\":\"mock-model\"}").unwrap();
    let work = dir.join("work");
    std::fs::create_dir(&work).unwrap();

    let cfg_path = dir.join("config.json");
    let cfg = json!({
        "token": "TESTTOKEN",
        "chat_id": "555",
        "owner_id": "777",
        "gray_bin": fake.to_string_lossy(),
        "gray_home": ghome.to_string_lossy(),
        "workdir": work.to_string_lossy(),
        "restart_notification": false,
        "concurrency": 1,
        "timeout_seconds": 60,
    });
    crate::config::atomic_json(&cfg_path, &cfg).unwrap();

    let mock = MockApi::start();
    std::env::set_var("GRAY_TELEGRAM_TEST_API_BASE", &mock.base);

    let run_path = cfg_path.clone();
    let daemon = tokio::spawn(async move { crate::gateway::run(&run_path).await });

    // Boot: deleteWebhook + getMe hit the mock.
    assert!(
        mock.wait_for("getMe", |_| true, 10),
        "daemon never called getMe"
    );

    // Update 1: plain text DM from the owner.
    mock.push(json!({
        "update_id": 1,
        "message": {
            "message_id": 10,
            "date": 1,
            "chat": {"id": 555, "type": "private"},
            "from": {"id": 777, "is_bot": false, "first_name": "A"},
            "text": "hello telegram e2e"
        }
    }));
    // Update 2: document attachment with a caption -> getFile + download.
    mock.push(json!({
        "update_id": 2,
        "message": {
            "message_id": 11,
            "date": 2,
            "chat": {"id": 555, "type": "private"},
            "from": {"id": 777, "is_bot": false, "first_name": "A"},
            "caption": "read this",
            "document": {"file_id": "FID42", "file_name": "readme.txt"}
        }
    }));

    // The turn's answer must arrive at the mock's sendMessage.
    assert!(
        mock.wait_for(
            "sendMessage",
            |b| b.get("text")
                .and_then(Value::as_str)
                .is_some_and(|t| t.contains("E2E MOCK ANSWER")),
            20
        ),
        "answer never reached sendMessage on the mock"
    );

    // Inbox rows: queued -> ran -> delivered ('sent').
    let store = crate::gateway::open_store(&cfg_path).unwrap();
    let row = store.get("tg:1").unwrap().expect("tg:1 not enqueued");
    assert_eq!(row.prompt, "hello telegram e2e");
    assert_eq!(row.state, "sent", "tg:1 state: {}", row.state);

    // The attachment row: prompt names the downloaded file; bytes landed
    // under workdir/attachments via getFile + /file/bot<tok>/<path>.
    let row2 = store.get("tg:2").unwrap().expect("tg:2 not enqueued");
    assert!(
        row2.prompt.contains("[attached file:"),
        "tg:2 prompt: {}",
        row2.prompt
    );
    let att = work.join("attachments").join("2-readme.txt");
    assert_eq!(
        std::fs::read_to_string(&att).unwrap(),
        "hello file e2e\n",
        "attachment bytes did not come from the mock"
    );
    assert!(
        mock.wait_for(
            "sendMessage",
            |b| b.get("chat_id") == Some(&json!("555")),
            5
        ),
        "no sendMessage to the home chat"
    );

    // telegram_send through the real sidecar dispatch -> sendMessage on mock.
    let res = crate::sidecar::dispatch(
        "tool/call",
        &json!({"name": "telegram_send", "args": {"content": "tool e2e hello"}}),
        &cfg_path,
    )
    .await;
    assert!(
        res.get("content")
            .and_then(Value::as_str)
            .is_some_and(|c| c.contains("Sent")),
        "telegram_send result: {res}"
    );
    assert!(
        mock.wait_for(
            "sendMessage",
            |b| b.get("text") == Some(&json!("tool e2e hello")),
            5
        ),
        "telegram_send never hit sendMessage on the mock"
    );

    daemon.abort();
}
