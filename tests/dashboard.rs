mod common;

use std::time::Duration;

use axum::http::Method;
use serde_json::{Value, json};

use common::{MockProvider, Reply, TEST_KEY, TestApp, completion, relay_error_code, remove_store_files};
use trout::codec::gzip;

async fn stats(app: &TestApp) -> Value {
    let response = app.admin(Method::GET, "/stats").send().await.unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");

    response.json().await.unwrap()
}

/// Traffic is recorded off the request path, so wait for it to land.
async fn stats_when(app: &TestApp, ready: impl Fn(&Value) -> bool) -> Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);

    loop {
        let current = stats(app).await;

        if ready(&current) || std::time::Instant::now() > deadline {
            return current;
        }

        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn page_is_public_static_and_self_contained() {
    let app = TestApp::start(|_| {}).await;
    let response = app.client.get(app.url("/dashboard")).send().await.unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/html; charset=utf-8"
    );
    assert_eq!(
        response.headers().get("x-content-type-options").unwrap(),
        "nosniff"
    );

    let policy = response
        .headers()
        .get("content-security-policy")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    assert!(policy.contains("default-src 'none'"), "{policy}");
    assert!(policy.contains("connect-src 'self'"), "{policy}");

    let page = response.text().await.unwrap();

    assert!(page.starts_with("<!doctype html>"));
    assert!(page.contains("/stats"));

    for forbidden in [
        "http://",
        "https://",
        "//cdn",
        "<link",
        "<script src",
        "<img",
        "@import",
        "url(",
        TEST_KEY,
    ] {
        assert!(!page.contains(forbidden), "page must not contain `{forbidden}`");
    }
}

#[tokio::test]
async fn stats_need_the_access_key() {
    let app = TestApp::start(|_| {}).await;
    let missing = app.client.get(app.url("/stats")).send().await.unwrap();

    assert_eq!(missing.status(), 401);
    assert_eq!(relay_error_code(missing).await, "unauthorized");

    let wrong = app
        .client
        .get(app.url("/stats"))
        .header("x-trout-key", "nope")
        .send()
        .await
        .unwrap();

    assert_eq!(wrong.status(), 401);
}

#[tokio::test]
async fn fresh_relay_reports_zeroes() {
    let app = TestApp::start(|config| config.store.max_bytes = 123_456_789).await;
    let current = stats(&app).await;

    assert_eq!(current["version"], env!("CARGO_PKG_VERSION"));
    assert!(current["generated_at_ms"].as_u64().unwrap() > 0);
    assert!(current["uptime_secs"].is_u64());
    assert_eq!(current["days"], json!([]));
    assert_eq!(current["store"]["conversations"], 0);
    assert_eq!(current["store"]["max_bytes"], 123_456_789);
    assert_eq!(current["store"]["ttl_secs"], 86_400);

    for field in [
        "proxy_requests",
        "response_turns",
        "conversations_started",
        "failed_requests",
        "client_up_bytes",
        "provider_up_bytes",
        "client_down_bytes",
        "plain_down_bytes",
        "upload_saved_bytes",
        "download_saved_bytes",
    ] {
        assert_eq!(current["totals"][field], 0, "{field}");
    }
}

#[tokio::test]
async fn compressed_proxy_request_is_counted_byte_for_byte() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let plain = json!({ "model": "m", "messages": [{ "role": "user", "content": "count me ".repeat(400) }] })
        .to_string();
    let packed = gzip(plain.as_bytes()).unwrap();
    let answer =
        json!({ "choices": [{ "message": { "role": "assistant", "content": "an answer ".repeat(300) } }] });
    let answer_len = serde_json::to_vec(&answer).unwrap().len() as u64;

    mock.push(Reply::Json {
        status: 200,
        body: answer,
    });

    let response = app
        .chat(&mock.target())
        .header("content-encoding", "gzip")
        .header("accept-encoding", "gzip")
        .body(packed.clone())
        .send()
        .await
        .unwrap();
    let received = response.bytes().await.unwrap().len() as u64;
    let current = stats_when(&app, |s| s["totals"]["proxy_requests"] == 1).await;
    let totals = &current["totals"];

    assert_eq!(totals["proxy_requests"], 1);
    assert_eq!(totals["response_turns"], 0);
    assert_eq!(totals["failed_requests"], 0);
    assert_eq!(totals["client_up_bytes"], packed.len());
    assert_eq!(totals["provider_up_bytes"], plain.len());
    assert_eq!(totals["upload_saved_bytes"], plain.len() - packed.len());
    assert_eq!(totals["client_down_bytes"], received);
    assert_eq!(totals["plain_down_bytes"], answer_len);
    assert_eq!(totals["download_saved_bytes"], answer_len - received);
    assert!(received < answer_len);
}

#[tokio::test]
async fn uncompressed_proxy_request_saves_nothing() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;

    mock.push(Reply::Json {
        status: 200,
        body: json!({ "ok": true }),
    });

    let response = app.chat(&mock.target()).send().await.unwrap();
    let received = response.bytes().await.unwrap().len() as u64;
    let current = stats_when(&app, |s| s["totals"]["proxy_requests"] == 1).await;
    let totals = &current["totals"];

    assert_eq!(totals["client_up_bytes"], totals["provider_up_bytes"]);
    assert_eq!(totals["upload_saved_bytes"], 0);
    assert_eq!(totals["client_down_bytes"], received);
    assert_eq!(totals["plain_down_bytes"], received);
    assert_eq!(totals["download_saved_bytes"], 0);
}

#[tokio::test]
async fn conversations_turns_failures_and_store_are_reported() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let target = mock.target();
    let tools: Vec<Value> = (0..40)
        .map(|i| json!({ "type": "function", "name": format!("Tool{i}"), "description": "Does a thing with a file. ".repeat(8) }))
        .collect();

    mock.push(completion("first answer"));
    mock.push(completion("second answer"));
    mock.push(completion("third answer"));
    mock.push(Reply::Json {
        status: 500,
        body: json!({ "error": "provider broke" }),
    });

    let first = app
        .turn(
            &target,
            json!({ "model": "m", "instructions": "Be brief.", "tools": tools, "input": "one" }),
        )
        .await;
    let second = app
        .turn(
            &target,
            json!({ "previous_response_id": first["id"], "input": "two" }),
        )
        .await;

    app.turn(
        &target,
        json!({ "previous_response_id": second["id"], "input": "three" }),
    )
    .await;

    let provider_error = app.chat(&target).send().await.unwrap();

    assert_eq!(provider_error.status(), 500);
    provider_error.bytes().await.unwrap();

    let rejected = app.chat("https://127.0.0.1:9/v1").send().await.unwrap();

    assert_eq!(rejected.status(), 403);
    rejected.bytes().await.unwrap();

    app.admin(
        Method::GET,
        &format!("/v1/responses/{}", first["id"].as_str().unwrap()),
    )
    .send()
    .await
    .unwrap();

    let current = stats_when(&app, |s| {
        s["totals"]["response_turns"] == 3 && s["totals"]["failed_requests"] == 2
    })
    .await;
    let totals = &current["totals"];

    assert_eq!(totals["conversations_started"], 1);
    assert_eq!(totals["response_turns"], 3);
    assert_eq!(
        totals["proxy_requests"], 1,
        "only the forwarded proxy request counts"
    );
    assert_eq!(totals["failed_requests"], 2);

    let (sent, would) = (
        totals["client_up_bytes"].as_u64().unwrap(),
        totals["provider_up_bytes"].as_u64().unwrap(),
    );

    assert!(
        would > sent * 2,
        "follow-ups should upload far less than the provider received: {sent} vs {would}"
    );
    assert_eq!(totals["upload_saved_bytes"], would - sent);

    assert_eq!(current["store"]["conversations"], 1);
    assert_eq!(current["store"]["responses"], 3);
    assert!(current["store"]["bytes"].as_u64().unwrap() > 0);

    let days = current["days"].as_array().unwrap();

    assert_eq!(days.len(), 1);
    assert_eq!(days[0]["response_turns"], 3);
    assert_eq!(days[0]["upload_saved_bytes"], totals["upload_saved_bytes"]);

    let day = days[0]["day"].as_str().unwrap();

    assert_eq!(day.len(), 10);
    assert!(
        day.chars().enumerate().all(|(i, c)| if i == 4 || i == 7 {
            c == '-'
        } else {
            c.is_ascii_digit()
        }),
        "{day}"
    );
}

#[tokio::test]
async fn unstored_turns_count_as_turns_but_not_as_conversations() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;

    mock.push(completion("ephemeral"));
    app.turn(
        &mock.target(),
        json!({ "model": "m", "input": "one-off", "store": false }),
    )
    .await;

    let current = stats_when(&app, |s| s["totals"]["response_turns"] == 1).await;

    assert_eq!(current["totals"]["response_turns"], 1);
    assert_eq!(current["totals"]["conversations_started"], 0);
    assert_eq!(current["store"]["conversations"], 0);
}

#[tokio::test]
async fn statistics_survive_a_restart() {
    let mock = MockProvider::start().await;
    let target = mock.target();
    let app = TestApp::with_mock(&mock).await;

    mock.push(completion("before"));
    app.turn(&target, json!({ "model": "m", "input": "remember the numbers" }))
        .await;

    let before = stats_when(&app, |s| s["totals"]["response_turns"] == 1).await;
    let path = app.stop();
    let entry = mock.entry();
    let restarted = TestApp::start_on(path.clone(), move |config| config.targets.list.push(entry)).await;
    let after = stats(&restarted).await;

    assert_eq!(after["totals"], before["totals"]);
    assert_eq!(after["days"], before["days"]);
    assert_eq!(after["store"]["conversations"], 1);

    drop(restarted);
    remove_store_files(&path);
}

#[tokio::test]
async fn dashboard_and_stats_requests_are_not_counted() {
    let app = TestApp::start(|_| {}).await;

    for _ in 0..3 {
        app.client
            .get(app.url("/dashboard"))
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        stats(&app).await;
        app.client
            .get(app.url("/healthz"))
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
    }

    tokio::time::sleep(Duration::from_millis(150)).await;

    let current = stats(&app).await;

    assert_eq!(current["days"], json!([]));
    assert_eq!(current["totals"]["failed_requests"], 0);
}
