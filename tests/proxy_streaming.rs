mod common;

use std::time::{Duration, Instant};

use serde_json::json;

use common::{MockProvider, Reply, TestApp, relay_error_code, sse_chunks};
use trout::config::TargetEntry;

const GAP: Duration = Duration::from_millis(200);

#[tokio::test]
async fn streamed_answers_arrive_piece_by_piece() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;

    mock.push(Reply::Sse {
        chunks: sse_chunks(4),
        gap: GAP,
    });

    let started = Instant::now();
    let mut response = app.chat(&mock.target()).send().await.unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    assert_eq!(response.headers().get("x-trout-source").unwrap(), "upstream");

    let first = response.chunk().await.unwrap().unwrap();
    let first_at = started.elapsed();
    let mut all = first.to_vec();

    while let Some(chunk) = response.chunk().await.unwrap() {
        all.extend_from_slice(&chunk);
    }

    let total = started.elapsed();

    assert_eq!(first.as_ref(), b"data: {\"i\":0}\n\n");
    assert_eq!(String::from_utf8(all).unwrap(), sse_chunks(4).concat());
    assert!(first_at < GAP * 2, "first piece took {first_at:?}");
    assert!(total >= GAP * 3, "whole answer took only {total:?}");
}

#[tokio::test]
async fn client_disconnect_stops_the_provider_request() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;

    mock.push(Reply::Sse {
        chunks: sse_chunks(50),
        gap: Duration::from_millis(50),
    });

    let mut response = app.chat(&mock.target()).send().await.unwrap();

    response.chunk().await.unwrap().unwrap();
    drop(response);

    let deadline = Instant::now() + Duration::from_secs(2);

    while !mock.stream_dropped_early() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    assert!(
        mock.stream_dropped_early(),
        "provider stream kept running after the client left"
    );
}

#[tokio::test]
async fn silent_provider_times_out() {
    let mock = MockProvider::start().await;
    let entry = mock.entry();
    let app = TestApp::start(move |config| {
        config.targets.list.push(entry);
        config.limits.first_byte_timeout_secs = 1;
    })
    .await;

    mock.push(Reply::Hang);

    let response = app.chat(&mock.target()).send().await.unwrap();

    assert_eq!(response.status(), 504);
    assert_eq!(relay_error_code(response).await, "upstream_timeout");
}

#[tokio::test]
async fn stalled_stream_ends_with_an_error_event() {
    let mock = MockProvider::start().await;
    let entry = mock.entry();
    let app = TestApp::start(move |config| {
        config.targets.list.push(entry);
        config.limits.stream_idle_timeout_secs = 1;
    })
    .await;

    mock.push(Reply::Sse {
        chunks: sse_chunks(2),
        gap: Duration::from_secs(3),
    });

    let response = app.chat(&mock.target()).send().await.unwrap();

    assert_eq!(response.status(), 200);

    let text = response.text().await.unwrap();

    assert!(text.starts_with("data: {\"i\":0}\n\n"), "{text}");
    assert!(text.contains("event: error\n"), "{text}");
    assert!(text.contains("\"code\":\"upstream_timeout\""), "{text}");
    assert!(!text.contains("\"i\":1"), "{text}");
}

#[tokio::test]
async fn unreachable_provider_is_a_relay_error() {
    let closed = TargetEntry {
        base_url: "http://127.0.0.1:1/v1".to_string(),
        accepts_gzip: false,
        allow_private: true,
    };
    let app = TestApp::start(move |config| config.targets.list.push(closed)).await;
    let response = app.chat("http://127.0.0.1:1/v1").send().await.unwrap();

    assert_eq!(response.status(), 502);
    assert_eq!(relay_error_code(response).await, "upstream_unreachable");
}

#[tokio::test]
async fn redirects_are_returned_not_followed() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;

    mock.push(Reply::Redirect {
        location: format!("http://{}/v1/elsewhere", mock.addr),
    });
    mock.push(Reply::Json {
        status: 200,
        body: json!({ "followed": true }),
    });

    let response = app.chat(&mock.target()).send().await.unwrap();

    assert_eq!(response.status(), 302);
    assert_eq!(response.headers().get("x-trout-source").unwrap(), "upstream");
    assert!(
        response
            .headers()
            .get("location")
            .unwrap()
            .to_str()
            .unwrap()
            .ends_with("/v1/elsewhere")
    );
    assert_eq!(mock.request_count(), 1);
}
