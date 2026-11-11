mod common;

use axum::http::Method;

use common::{MockProvider, TEST_KEY, TestApp, relay_error_code};
use trout::config::TargetMode;

async fn code_for(app: &TestApp, target: &str) -> (u16, String) {
    let response = app.chat(target).send().await.unwrap();
    let status = response.status().as_u16();

    (status, relay_error_code(response).await)
}

#[tokio::test]
async fn missing_target_is_rejected() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let response = app
        .client
        .request(Method::POST, app.url("/v1/chat/completions"))
        .header("x-trout-key", TEST_KEY)
        .body("{}")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 400);
    assert_eq!(relay_error_code(response).await, "target_missing");
    assert_eq!(mock.request_count(), 0);
}

#[tokio::test]
async fn malformed_targets_are_invalid() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;

    for target in [
        "/v1",
        "api.example.com/v1",
        "ftp://api.example.com/v1",
        "https://user:pw@api.example.com/v1",
        "https://api.example.com/v1?key=1",
        "http://api.example.com/v1",
    ] {
        assert_eq!(
            code_for(&app, target).await,
            (400, "target_invalid".to_string()),
            "{target}"
        );
    }

    assert_eq!(mock.request_count(), 0);
}

#[tokio::test]
async fn non_public_targets_are_forbidden_unless_listed() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;

    for target in [
        "https://127.0.0.1:9/v1",
        "https://10.0.0.1/v1",
        "https://192.168.1.10/v1",
        "https://[::1]:9/v1",
        "https://localhost:9/v1",
    ] {
        assert_eq!(
            code_for(&app, target).await,
            (403, "target_forbidden".to_string()),
            "{target}"
        );
    }

    let unlisted_port = format!("http://127.0.0.1:{}/v1", mock.addr.port().wrapping_add(1));

    assert_eq!(code_for(&app, &unlisted_port).await.1, "target_invalid");
    assert_eq!(mock.request_count(), 0);
}

#[tokio::test]
async fn listed_mode_refuses_unlisted_public_targets() {
    let mock = MockProvider::start().await;
    let entry = mock.entry();
    let app = TestApp::start(move |config| {
        config.targets.mode = TargetMode::Listed;
        config.targets.list.push(entry);
    })
    .await;

    assert_eq!(
        code_for(&app, "https://api.example.com/v1").await,
        (403, "target_forbidden".to_string())
    );
    assert_eq!(mock.request_count(), 0);
}
