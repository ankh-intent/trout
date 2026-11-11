#![allow(dead_code)]

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use futures_util::stream;
use tokio::net::TcpListener;

use trout::config::{Config, TargetEntry};

pub const TEST_KEY: &str = "test-access-key-0123456789abcdef-0123456789";
pub const PROVIDER_KEY: &str = "Bearer sk-provider-secret";

/// What the mock provider answers with, one per incoming request, in order.
pub enum Reply {
    Json {
        status: u16,
        body: serde_json::Value,
    },
    /// `text/event-stream` chunks written `gap` apart.
    Sse {
        chunks: Vec<String>,
        gap: Duration,
    },
    Raw {
        status: u16,
        headers: Vec<(&'static str, String)>,
        body: Vec<u8>,
    },
    Redirect {
        location: String,
    },
    /// Never answers.
    Hang,
}

#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: Method,
    pub path_and_query: String,
    pub headers: HeaderMap,
    pub body: Bytes,
}

struct MockState {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<Recorded>>,
    stream_dropped_early: Arc<AtomicBool>,
}

/// A scripted OpenAI-compatible provider on an ephemeral loopback port.
pub struct MockProvider {
    pub addr: SocketAddr,
    state: Arc<MockState>,
}

impl MockProvider {
    pub async fn start() -> Self {
        let state = Arc::new(MockState {
            replies: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
            stream_dropped_early: Arc::new(AtomicBool::new(false)),
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().fallback(mock_handler).with_state(state.clone());

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        Self { addr, state }
    }

    pub fn target(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    pub fn entry(&self) -> TargetEntry {
        TargetEntry {
            base_url: self.target(),
            accepts_gzip: false,
            allow_private: true,
        }
    }

    pub fn push(&self, reply: Reply) {
        self.state.replies.lock().unwrap().push_back(reply);
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.state.requests.lock().unwrap().clone()
    }

    pub fn request_count(&self) -> usize {
        self.state.requests.lock().unwrap().len()
    }

    /// True once a streamed answer was abandoned before its last chunk was written.
    pub fn stream_dropped_early(&self) -> bool {
        self.state.stream_dropped_early.load(Ordering::SeqCst)
    }
}

struct DropGuard {
    flag: Arc<AtomicBool>,
    finished: AtomicBool,
}

impl Drop for DropGuard {
    fn drop(&mut self) {
        if !self.finished.load(Ordering::SeqCst) {
            self.flag.store(true, Ordering::SeqCst);
        }
    }
}

async fn mock_handler(State(state): State<Arc<MockState>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();

    state.requests.lock().unwrap().push(Recorded {
        method: parts.method.clone(),
        path_and_query: parts
            .uri
            .path_and_query()
            .map(|p| p.as_str().to_string())
            .unwrap_or_default(),
        headers: parts.headers.clone(),
        body,
    });

    let reply = state.replies.lock().unwrap().pop_front();

    match reply {
        None => json_response(500, &serde_json::json!({ "error": "mock: no reply scripted" })),
        Some(Reply::Json { status, body }) => json_response(status, &body),
        Some(Reply::Raw {
            status,
            headers,
            body,
        }) => {
            let mut response = Response::new(Body::from(body));

            *response.status_mut() = StatusCode::from_u16(status).unwrap();

            for (name, value) in headers {
                response.headers_mut().append(
                    HeaderName::from_static(name),
                    HeaderValue::from_str(&value).unwrap(),
                );
            }

            response
        }
        Some(Reply::Redirect { location }) => {
            let mut response = Response::new(Body::empty());

            *response.status_mut() = StatusCode::FOUND;
            response
                .headers_mut()
                .insert("location", HeaderValue::from_str(&location).unwrap());

            response
        }
        Some(Reply::Hang) => std::future::pending().await,
        Some(Reply::Sse { chunks, gap }) => {
            let guard = DropGuard {
                flag: state.stream_dropped_early.clone(),
                finished: AtomicBool::new(false),
            };
            let chunks: VecDeque<String> = chunks.into();
            let body = stream::unfold(
                (chunks, guard, true),
                move |(mut chunks, guard, first)| async move {
                    if !first {
                        tokio::time::sleep(gap).await;
                    }

                    match chunks.pop_front() {
                        Some(chunk) => Some((
                            Ok::<_, std::io::Error>(Bytes::from(chunk)),
                            (chunks, guard, false),
                        )),
                        None => {
                            guard.finished.store(true, Ordering::SeqCst);

                            None
                        }
                    }
                },
            );
            let mut response = Response::new(Body::from_stream(body));

            response
                .headers_mut()
                .insert("content-type", HeaderValue::from_static("text/event-stream"));

            response
        }
    }
}

fn json_response(status: u16, body: &serde_json::Value) -> Response {
    let mut response = Response::new(Body::from(serde_json::to_vec(body).unwrap()));

    *response.status_mut() = StatusCode::from_u16(status).unwrap();
    response
        .headers_mut()
        .insert("content-type", HeaderValue::from_static("application/json"));

    response
}

/// The real router on an ephemeral port.
pub struct TestApp {
    pub addr: SocketAddr,
    pub client: reqwest::Client,
    pub store_path: PathBuf,
    server: tokio::task::JoinHandle<()>,
    remove_store: bool,
}

impl Drop for TestApp {
    fn drop(&mut self) {
        self.server.abort();

        if self.remove_store {
            remove_store_files(&self.store_path);
        }
    }
}

pub fn remove_store_files(path: &std::path::Path) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

impl TestApp {
    /// Starts on a fresh store. `customise` runs after validation, so tests may set values the
    /// operator contract would refuse (a one-second retention period, a tiny store).
    pub async fn start(customise: impl FnOnce(&mut Config)) -> Self {
        let path = std::env::temp_dir().join(format!("trout-test-{}.db", uuid::Uuid::new_v4().simple()));

        Self::start_on(path, customise).await
    }

    pub async fn start_on(store_path: PathBuf, customise: impl FnOnce(&mut Config)) -> Self {
        let mut config = Config {
            access_key: TEST_KEY.to_string(),
            ..Config::default()
        };

        config.validate().unwrap();
        config.store.path = store_path.clone();
        customise(&mut config);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = trout::build_app(config).unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();

        Self {
            addr,
            client,
            store_path,
            server,
            remove_store: true,
        }
    }

    /// Stops the server and leaves the store on disk for a later `start_on`.
    pub fn stop(mut self) -> PathBuf {
        self.remove_store = false;

        self.store_path.clone()
    }

    /// An app that knows one mock provider as a private, listed target.
    pub async fn with_mock(mock: &MockProvider) -> Self {
        let entry = mock.entry();

        Self::start(move |config| config.targets.list.push(entry)).await
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    /// A request carrying the relay key, the target and a provider credential.
    pub fn request(&self, method: Method, path: &str, target: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, self.url(path))
            .header("x-trout-key", TEST_KEY)
            .header("x-trout-target", target)
            .header("authorization", PROVIDER_KEY)
    }

    pub fn chat(&self, target: &str) -> reqwest::RequestBuilder {
        self.request(Method::POST, "/v1/chat/completions", target)
            .header("content-type", "application/json")
            .body(r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#)
    }
}

pub async fn relay_error_code(response: reqwest::Response) -> String {
    assert_eq!(response.headers().get("x-trout-source").unwrap(), "relay");

    let body: serde_json::Value = response.json().await.unwrap();

    assert_eq!(body["error"]["type"], "trout_error");

    body["error"]["code"].as_str().unwrap().to_string()
}

pub fn sse_chunks(count: usize) -> Vec<String> {
    (0..count).map(|i| format!("data: {{\"i\":{i}}}\n\n")).collect()
}

pub fn gunzip(data: &[u8]) -> Vec<u8> {
    use std::io::Read;

    let mut out = Vec::new();

    flate2::read::GzDecoder::new(data).read_to_end(&mut out).unwrap();

    out
}

/// A non-streamed chat completion as a provider would return it.
pub fn completion(content: &str) -> Reply {
    Reply::Json {
        status: 200,
        body: serde_json::json!({
            "id": "chatcmpl-1",
            "choices": [{ "index": 0, "finish_reason": "stop", "message": { "role": "assistant", "content": content } }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 3, "total_tokens": 13 },
        }),
    }
}

pub fn completion_with_calls(calls: &[(&str, &str, &str)]) -> Reply {
    let calls: Vec<serde_json::Value> = calls
        .iter()
        .map(|(id, name, arguments)| {
            serde_json::json!({ "id": id, "type": "function", "function": { "name": name, "arguments": arguments } })
        })
        .collect();

    Reply::Json {
        status: 200,
        body: serde_json::json!({
            "choices": [{ "index": 0, "finish_reason": "tool_calls", "message": { "role": "assistant", "content": null, "tool_calls": calls } }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 3, "total_tokens": 13 },
        }),
    }
}

/// One `data:` line of a streamed chat completion.
pub fn chat_chunk(delta: serde_json::Value, finish_reason: Option<&str>) -> String {
    format!(
        "data: {}\n\n",
        serde_json::json!({ "choices": [{ "index": 0, "delta": delta, "finish_reason": finish_reason }] })
    )
}

pub fn usage_chunk() -> String {
    format!(
        "data: {}\n\n",
        serde_json::json!({ "choices": [], "usage": { "prompt_tokens": 10, "completion_tokens": 3, "total_tokens": 13 } })
    )
}

pub const DONE: &str = "data: [DONE]\n\n";

/// Splits an event-stream body into `(event name, data)` pairs.
pub fn sse_events(text: &str) -> Vec<(String, serde_json::Value)> {
    text.split("\n\n")
        .filter(|block| !block.trim().is_empty())
        .map(|block| {
            let mut name = String::new();
            let mut data = serde_json::Value::Null;

            for line in block.lines() {
                if let Some(value) = line.strip_prefix("event: ") {
                    name = value.to_string();
                } else if let Some(value) = line.strip_prefix("data: ") {
                    data = serde_json::from_str(value).unwrap();
                }
            }

            (name, data)
        })
        .collect()
}

impl TestApp {
    /// `POST /v1/responses` with the relay key, the target and a provider credential.
    pub fn responses(&self, target: &str, body: serde_json::Value) -> reqwest::RequestBuilder {
        self.request(Method::POST, "/v1/responses", target)
            .header("content-type", "application/json")
            .body(body.to_string())
    }

    pub async fn turn(&self, target: &str, body: serde_json::Value) -> serde_json::Value {
        let response = self.responses(target, body).send().await.unwrap();

        assert_eq!(response.status(), 200, "turn failed");

        response.json().await.unwrap()
    }

    /// A request to a relay-owned route that needs no target.
    pub fn admin(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, self.url(path))
            .header("x-trout-key", TEST_KEY)
    }
}

/// The JSON body the mock provider received for its `index`-th request.
pub fn received(mock: &MockProvider, index: usize) -> serde_json::Value {
    serde_json::from_slice(&mock.requests()[index].body).unwrap()
}
