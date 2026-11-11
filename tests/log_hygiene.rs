mod common;

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::Method;
use serde_json::json;
use tracing_subscriber::fmt::MakeWriter;

use common::{MockProvider, TEST_KEY, TestApp, completion};

const SECRET_PROVIDER_KEY: &str = "Bearer sk-HYGIENE-PROVIDER-SECRET-9f3a";
const SECRET_QUESTION: &str = "HYGIENE-QUESTION-TEXT-71c2";
const SECRET_ANSWER: &str = "HYGIENE-ANSWER-TEXT-b84e";
const SECRET_INSTRUCTIONS: &str = "HYGIENE-INSTRUCTIONS-TEXT-0d55";

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);

        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Capture;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl Capture {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }

    fn request_lines(&self) -> Vec<serde_json::Value> {
        self.text()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|line| line["target"] == "trout::request")
            .map(|line| line["fields"].clone())
            .collect()
    }
}

// One test only: it installs the process-wide log subscriber.
#[tokio::test]
async fn one_line_per_request_and_no_secrets_or_content() {
    let capture = Capture::default();

    tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(capture.clone())
        .init();

    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let target = mock.target();
    let chat_body =
        json!({ "model": "m", "messages": [{ "role": "user", "content": SECRET_QUESTION }] }).to_string();

    for _ in 0..3 {
        mock.push(completion(SECRET_ANSWER));
    }

    let plain = app
        .request(Method::POST, "/v1/chat/completions", &target)
        .header("authorization", SECRET_PROVIDER_KEY)
        .header("content-type", "application/json")
        .body(chat_body.clone())
        .send()
        .await
        .unwrap();

    assert_eq!(plain.status(), 200);
    plain.bytes().await.unwrap();

    let packed = trout::codec::gzip(chat_body.as_bytes()).unwrap();
    let packed_len = packed.len();
    let gzipped = app
        .request(Method::POST, "/v1/chat/completions", &target)
        .header("authorization", SECRET_PROVIDER_KEY)
        .header("content-type", "application/json")
        .header("content-encoding", "gzip")
        .header("accept-encoding", "gzip")
        .body(packed)
        .send()
        .await
        .unwrap();

    assert_eq!(gzipped.status(), 200);
    gzipped.bytes().await.unwrap();

    let turn = app
        .responses(
            &target,
            json!({ "model": "m", "instructions": SECRET_INSTRUCTIONS, "input": SECRET_QUESTION }),
        )
        .header("authorization", SECRET_PROVIDER_KEY)
        .send()
        .await
        .unwrap();

    assert_eq!(turn.status(), 200);
    turn.bytes().await.unwrap();

    let rejected = app
        .client
        .get(app.url("/v1/models"))
        .header("x-trout-key", "wrong-key")
        .send()
        .await
        .unwrap();

    assert_eq!(rejected.status(), 401);
    rejected.bytes().await.unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(2);

    while capture.request_lines().len() < 4 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let lines = capture.request_lines();
    let text = capture.text();

    assert_eq!(lines.len(), 4, "expected exactly one line per request:\n{text}");

    for secret in [
        SECRET_PROVIDER_KEY,
        "sk-HYGIENE",
        SECRET_QUESTION,
        SECRET_ANSWER,
        SECRET_INSTRUCTIONS,
        TEST_KEY,
        "wrong-key",
    ] {
        assert!(!text.contains(secret), "log contains `{secret}`:\n{text}");
    }

    let (plain, gzipped, turn, rejected) = (&lines[0], &lines[1], &lines[2], &lines[3]);

    for line in [plain, gzipped, turn] {
        assert_eq!(line["target_host"], "127.0.0.1");
        assert_eq!(line["status"], 200);
        assert_eq!(line["method"], "POST");
        assert!(line["request_id"].as_str().is_some_and(|id| id.len() == 32));
        assert!(line["duration_ms"].is_u64());
        assert!(line["ttfb_ms"].is_u64());
        assert!(line["bytes_out"].as_u64().unwrap() > 0);
    }

    assert_eq!(plain["route"], "proxy");
    assert_eq!(plain["source"], "upstream");
    assert_eq!(plain["bytes_in"], chat_body.len());
    assert_eq!(plain["bytes_in_decoded"], chat_body.len());

    assert_eq!(gzipped["bytes_in"], packed_len);
    assert_eq!(gzipped["bytes_in_decoded"], chat_body.len());

    assert_eq!(turn["route"], "responses");
    assert_eq!(turn["source"], "relay");

    assert_eq!(rejected["status"], 401);
    assert_eq!(rejected["source"], "relay");
    assert_eq!(rejected["target_host"], "");
}
