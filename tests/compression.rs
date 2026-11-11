mod common;

use serde_json::json;

use common::{MockProvider, Reply, TestApp, relay_error_code};
use trout::codec::gzip;
use trout::config::TargetEntry;

const BODY: &str =
    r#"{"model":"m","messages":[{"role":"user","content":"compress me, compress me, compress me"}]}"#;

#[tokio::test]
async fn gzip_request_reaches_the_provider_uncompressed_and_identical() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;

    mock.push(Reply::Json {
        status: 200,
        body: json!({ "ok": true }),
    });

    let response = app
        .chat(&mock.target())
        .header("content-encoding", "gzip")
        .body(gzip(BODY.as_bytes()).unwrap())
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);

    let seen = mock.requests();

    assert_eq!(seen[0].body, BODY.as_bytes());
    assert!(!seen[0].headers.contains_key("content-encoding"));
    assert_eq!(
        seen[0].headers.get("content-length").unwrap(),
        BODY.len().to_string().as_str()
    );
    assert_eq!(seen[0].headers.get("accept-encoding").unwrap(), "gzip");
}

#[tokio::test]
async fn target_that_accepts_gzip_receives_gzip() {
    let mock = MockProvider::start().await;
    let entry = TargetEntry {
        accepts_gzip: true,
        ..mock.entry()
    };
    let app = TestApp::start(move |config| config.targets.list.push(entry)).await;

    mock.push(Reply::Json {
        status: 200,
        body: json!({}),
    });
    mock.push(Reply::Json {
        status: 200,
        body: json!({}),
    });

    app.chat(&mock.target()).body(BODY).send().await.unwrap();
    app.chat(&mock.target())
        .header("content-encoding", "gzip")
        .body(gzip(BODY.as_bytes()).unwrap())
        .send()
        .await
        .unwrap();

    for seen in mock.requests() {
        assert_eq!(seen.headers.get("content-encoding").unwrap(), "gzip");
        assert_ne!(seen.body, BODY.as_bytes());
        assert_eq!(common::gunzip(&seen.body), BODY.as_bytes());
    }
}

#[tokio::test]
async fn corrupt_gzip_is_rejected() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let response = app
        .chat(&mock.target())
        .header("content-encoding", "gzip")
        .body(BODY)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 400);
    assert_eq!(relay_error_code(response).await, "body_corrupt");
    assert_eq!(mock.request_count(), 0);
}

#[tokio::test]
async fn other_encodings_are_unsupported_and_gzip_is_named() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let response = app
        .chat(&mock.target())
        .header("content-encoding", "br")
        .body(BODY)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 415);
    assert_eq!(response.headers().get("accept-encoding").unwrap(), "gzip");
    assert_eq!(relay_error_code(response).await, "encoding_unsupported");
    assert_eq!(mock.request_count(), 0);
}

#[tokio::test]
async fn small_body_that_expands_past_the_decoded_limit_is_refused() {
    let mock = MockProvider::start().await;
    let entry = mock.entry();
    let app = TestApp::start(move |config| {
        config.targets.list.push(entry);
        config.limits.max_body_bytes = 16 * 1024;
        config.limits.max_decoded_body_bytes = 64 * 1024;
    })
    .await;
    let bomb = gzip(&vec![b' '; 4 * 1024 * 1024]).unwrap();

    assert!(bomb.len() < 16 * 1024);

    let response = app
        .chat(&mock.target())
        .header("content-encoding", "gzip")
        .body(bomb)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 413);
    assert_eq!(relay_error_code(response).await, "body_too_large");
    assert_eq!(mock.request_count(), 0);
}
