mod common;

use common::{MockProvider, TEST_KEY, TestApp, relay_error_code};

#[tokio::test]
async fn health_needs_no_key_and_reveals_only_status() {
    let app = TestApp::start(|_| {}).await;
    let response = app.client.get(app.url("/healthz")).send().await.unwrap();

    assert_eq!(response.status(), 200);
    assert!(response.headers().contains_key("x-trout-request-id"));

    let body: serde_json::Value = response.json().await.unwrap();

    assert_eq!(body, serde_json::json!({ "status": "ok" }));
}

#[tokio::test]
async fn missing_or_wrong_key_is_rejected_before_anything_else() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let target = mock.target();

    let missing = app
        .client
        .post(app.url("/v1/chat/completions"))
        .header("x-trout-target", &target)
        .body("{}")
        .send()
        .await
        .unwrap();

    assert_eq!(missing.status(), 401);
    assert_eq!(relay_error_code(missing).await, "unauthorized");

    let wrong = app
        .client
        .post(app.url("/v1/chat/completions"))
        .header("x-trout-key", format!("{TEST_KEY}x"))
        .header("x-trout-target", &target)
        .body("{}")
        .send()
        .await
        .unwrap();

    assert_eq!(wrong.status(), 401);
    assert_eq!(relay_error_code(wrong).await, "unauthorized");

    let outside = app.client.get(app.url("/anything")).send().await.unwrap();

    assert_eq!(outside.status(), 401);
    assert_eq!(mock.request_count(), 0);
}

#[tokio::test]
async fn unknown_path_with_valid_key_is_route_not_found() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let response = app
        .client
        .get(app.url("/v2/models"))
        .header("x-trout-key", TEST_KEY)
        .header("x-trout-target", mock.target())
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 404);
    assert_eq!(relay_error_code(response).await, "route_not_found");
    assert_eq!(mock.request_count(), 0);
}
