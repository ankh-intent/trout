mod common;

use axum::http::Method;
use serde_json::json;

use common::{MockProvider, PROVIDER_KEY, Reply, TestApp, relay_error_code};

#[tokio::test]
async fn forwards_path_query_method_body_and_credential() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;

    mock.push(Reply::Json {
        status: 200,
        body: json!({ "id": "cmpl-1", "choices": [] }),
    });

    let body = r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#;
    let response = app
        .request(Method::POST, "/v1/chat/completions?x=1&y=a%20b", &mock.target())
        .header("content-type", "application/json")
        .header("x-custom", "kept")
        .body(body)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers().get("x-trout-source").unwrap(), "upstream");
    assert!(response.headers().contains_key("x-trout-request-id"));
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/json"
    );
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap(),
        json!({ "id": "cmpl-1", "choices": [] })
    );

    let seen = mock.requests();

    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, Method::POST);
    assert_eq!(seen[0].path_and_query, "/v1/chat/completions?x=1&y=a%20b");
    assert_eq!(seen[0].body, body.as_bytes());
    assert_eq!(seen[0].headers.get("authorization").unwrap(), PROVIDER_KEY);
    assert_eq!(seen[0].headers.get("content-type").unwrap(), "application/json");
    assert_eq!(seen[0].headers.get("x-custom").unwrap(), "kept");
    assert_eq!(
        seen[0].headers.get("host").unwrap(),
        mock.addr.to_string().as_str()
    );
}

#[tokio::test]
async fn relay_and_hop_headers_never_reach_the_provider() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;

    mock.push(Reply::Json {
        status: 200,
        body: json!({}),
    });

    app.chat(&mock.target())
        .header("x-trout-option", "1")
        .header("proxy-authorization", "Basic abc")
        .header("te", "trailers")
        .header("accept-encoding", "br")
        .send()
        .await
        .unwrap();

    let seen = mock.requests();
    let names: Vec<&str> = seen[0].headers.keys().map(|k| k.as_str()).collect();

    for name in &names {
        assert!(!name.starts_with("x-trout-"), "{name} leaked");
        assert!(!name.starts_with("proxy-"), "{name} leaked");
    }

    assert!(!names.contains(&"te"));
    assert!(!names.contains(&"content-encoding"));
    assert_ne!(
        seen[0]
            .headers
            .get("accept-encoding")
            .map(|v| v.to_str().unwrap()),
        Some("br")
    );
}

#[tokio::test]
async fn other_methods_and_paths_are_forwarded() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;

    mock.push(Reply::Json {
        status: 200,
        body: json!({ "data": [] }),
    });
    mock.push(Reply::Json {
        status: 200,
        body: json!({ "deleted": true }),
    });

    let list = app
        .request(Method::GET, "/v1/models", &mock.target())
        .send()
        .await
        .unwrap();
    let delete = app
        .request(Method::DELETE, "/v1/files/file-1", &mock.target())
        .send()
        .await
        .unwrap();

    assert_eq!(list.status(), 200);
    assert_eq!(delete.status(), 200);

    let seen = mock.requests();

    assert_eq!(
        (seen[0].method.clone(), seen[0].path_and_query.as_str()),
        (Method::GET, "/v1/models")
    );
    assert_eq!(
        (seen[1].method.clone(), seen[1].path_and_query.as_str()),
        (Method::DELETE, "/v1/files/file-1")
    );
}

#[tokio::test]
async fn provider_errors_come_back_unchanged() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let provider_error =
        json!({ "error": { "type": "bad_request_error", "message": "invalid params (2013)" } });

    mock.push(Reply::Raw {
        status: 429,
        headers: vec![
            ("content-type", "application/json".to_string()),
            ("retry-after", "7".to_string()),
        ],
        body: serde_json::to_vec(&provider_error).unwrap(),
    });

    let response = app.chat(&mock.target()).send().await.unwrap();

    assert_eq!(response.status(), 429);
    assert_eq!(response.headers().get("x-trout-source").unwrap(), "upstream");
    assert_eq!(response.headers().get("retry-after").unwrap(), "7");
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap(),
        provider_error
    );
}

#[tokio::test]
async fn oversized_bodies_are_refused_without_contacting_the_provider() {
    let mock = MockProvider::start().await;
    let entry = mock.entry();
    let app = TestApp::start(move |config| {
        config.targets.list.push(entry);
        config.limits.max_body_bytes = 1024;
    })
    .await;
    let big = vec![b'a'; 4096];

    let declared = app.chat(&mock.target()).body(big.clone()).send().await.unwrap();

    assert_eq!(declared.status(), 413);
    assert_eq!(relay_error_code(declared).await, "body_too_large");

    let pieces: Vec<Result<Vec<u8>, std::io::Error>> = big.chunks(512).map(|c| Ok(c.to_vec())).collect();
    let chunked = app
        .chat(&mock.target())
        .body(reqwest::Body::wrap_stream(futures_util::stream::iter(pieces)))
        .send()
        .await
        .unwrap();

    assert_eq!(chunked.status(), 413);
    assert_eq!(relay_error_code(chunked).await, "body_too_large");

    mock.push(Reply::Json {
        status: 200,
        body: json!({}),
    });

    let small = app.chat(&mock.target()).send().await.unwrap();

    assert_eq!(small.status(), 200);
    assert_eq!(mock.request_count(), 1);
}
