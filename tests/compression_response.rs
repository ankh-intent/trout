mod common;

use std::io::Write;
use std::time::{Duration, Instant};

use flate2::write::GzDecoder;
use serde_json::json;

use common::{MockProvider, Reply, TestApp, gunzip, relay_error_code, sse_chunks};
use trout::codec::gzip;

const GAP: Duration = Duration::from_millis(200);

fn answer() -> serde_json::Value {
    json!({ "id": "cmpl-1", "choices": [{ "message": { "role": "assistant", "content": "hello ".repeat(200) } }] })
}

#[tokio::test]
async fn client_that_accepts_gzip_gets_gzip() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;

    mock.push(Reply::Json {
        status: 200,
        body: answer(),
    });

    let response = app
        .chat(&mock.target())
        .header("accept-encoding", "gzip")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers().get("content-encoding").unwrap(), "gzip");
    assert!(
        response
            .headers()
            .get_all("vary")
            .iter()
            .any(|v| v == "accept-encoding")
    );
    assert!(!response.headers().contains_key("content-length"));

    let wire = response.bytes().await.unwrap();
    let plain = serde_json::to_vec(&answer()).unwrap();

    assert!(wire.len() < plain.len() / 2, "{} vs {}", wire.len(), plain.len());
    assert_eq!(gunzip(&wire), plain);
}

#[tokio::test]
async fn client_without_gzip_gets_plain_even_if_the_provider_compressed() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let plain = serde_json::to_vec(&answer()).unwrap();

    for _ in 0..2 {
        mock.push(Reply::Raw {
            status: 200,
            headers: vec![
                ("content-type", "application/json".to_string()),
                ("content-encoding", "gzip".to_string()),
            ],
            body: gzip(&plain).unwrap(),
        });
    }

    let without = app.chat(&mock.target()).send().await.unwrap();

    assert!(!without.headers().contains_key("content-encoding"));
    assert_eq!(without.bytes().await.unwrap(), plain);

    let refused = app
        .chat(&mock.target())
        .header("accept-encoding", "gzip;q=0, br")
        .send()
        .await
        .unwrap();

    assert!(!refused.headers().contains_key("content-encoding"));
    assert_eq!(refused.bytes().await.unwrap(), plain);
}

#[tokio::test]
async fn provider_gzip_is_recompressed_for_a_gzip_client() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let plain = serde_json::to_vec(&answer()).unwrap();

    mock.push(Reply::Raw {
        status: 200,
        headers: vec![
            ("content-type", "application/json".to_string()),
            ("content-encoding", "gzip".to_string()),
        ],
        body: gzip(&plain).unwrap(),
    });

    let response = app
        .chat(&mock.target())
        .header("accept-encoding", "gzip")
        .send()
        .await
        .unwrap();

    assert_eq!(response.headers().get_all("content-encoding").iter().count(), 1);
    assert_eq!(gunzip(&response.bytes().await.unwrap()), plain);
}

#[tokio::test]
async fn unknown_provider_encoding_is_a_relay_error() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;

    mock.push(Reply::Raw {
        status: 200,
        headers: vec![("content-encoding", "br".to_string())],
        body: b"not really brotli".to_vec(),
    });

    let response = app.chat(&mock.target()).send().await.unwrap();

    assert_eq!(response.status(), 502);
    assert_eq!(relay_error_code(response).await, "upstream_encoding_unsupported");
}

#[tokio::test]
async fn provider_errors_are_compressed_but_otherwise_unchanged() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let error = json!({ "error": { "message": "rate limited" } });

    mock.push(Reply::Json {
        status: 429,
        body: error.clone(),
    });

    let response = app
        .chat(&mock.target())
        .header("accept-encoding", "gzip")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 429);
    assert_eq!(response.headers().get("x-trout-source").unwrap(), "upstream");
    assert_eq!(
        gunzip(&response.bytes().await.unwrap()),
        serde_json::to_vec(&error).unwrap()
    );
}

#[tokio::test]
async fn compressed_stream_delivers_each_event_on_arrival() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;

    mock.push(Reply::Sse {
        chunks: sse_chunks(4),
        gap: GAP,
    });

    let started = Instant::now();
    let mut response = app
        .chat(&mock.target())
        .header("accept-encoding", "gzip")
        .send()
        .await
        .unwrap();

    assert_eq!(response.headers().get("content-encoding").unwrap(), "gzip");
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream"
    );

    let mut decoder = GzDecoder::new(Vec::new());
    let mut first_event_at = None;

    while let Some(chunk) = response.chunk().await.unwrap() {
        decoder.write_all(&chunk).unwrap();
        decoder.flush().unwrap();

        if first_event_at.is_none() && decoder.get_ref().starts_with(b"data: {\"i\":0}\n\n") {
            first_event_at = Some(started.elapsed());
        }
    }

    let total = started.elapsed();
    let decoded = decoder.finish().unwrap();

    assert_eq!(String::from_utf8(decoded).unwrap(), sse_chunks(4).concat());
    assert!(
        first_event_at.unwrap() < GAP * 2,
        "first event decodable only after {first_event_at:?}"
    );
    assert!(total >= GAP * 3, "whole answer took only {total:?}");
}

#[tokio::test]
async fn head_and_empty_answers_are_not_wrapped() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;

    mock.push(Reply::Raw {
        status: 204,
        headers: vec![],
        body: vec![],
    });

    let response = app
        .request(axum::http::Method::DELETE, "/v1/files/f1", &mock.target())
        .header("accept-encoding", "gzip")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 204);
    assert!(!response.headers().contains_key("content-encoding"));
    assert!(response.bytes().await.unwrap().is_empty());
}
