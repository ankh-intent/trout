mod common;

use common::{TEST_KEY, TestApp};

async fn page(app: &TestApp, path: &str) -> String {
    let response = app.client.get(app.url(path)).send().await.unwrap();

    assert_eq!(response.status(), 200, "{path}");
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/html; charset=utf-8",
        "{path}"
    );
    assert_eq!(
        response.headers().get("cache-control").unwrap(),
        "no-store",
        "{path}"
    );
    assert_eq!(
        response.headers().get("x-content-type-options").unwrap(),
        "nosniff",
        "{path}"
    );

    let policy = response
        .headers()
        .get("content-security-policy")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    assert!(policy.contains("default-src 'none'"), "{path}: {policy}");
    assert!(policy.contains("connect-src 'self'"), "{path}: {policy}");

    response.text().await.unwrap()
}

#[tokio::test]
async fn every_page_is_public_and_carries_the_nav_bar() {
    let app = TestApp::start(|_| {}).await;

    for (path, current) in [
        ("/", r#"<a href="/" aria-current="page">Home</a>"#),
        (
            "/dashboard",
            r#"<a href="/dashboard" aria-current="page">Dashboard</a>"#,
        ),
        (
            "/providers",
            r#"<a href="/providers" aria-current="page">LLM providers</a>"#,
        ),
    ] {
        let html = page(&app, path).await;

        assert!(html.starts_with("<!doctype html>"), "{path}");
        assert!(html.contains(r#"<nav class="topnav""#), "{path}");
        assert!(html.contains(current), "{path} should mark itself as current");
        assert_eq!(html.matches(r#"aria-current="page""#).count(), 1, "{path}");

        for link in [r#"href="/""#, r#"href="/dashboard""#, r#"href="/providers""#] {
            assert!(html.contains(link), "{path} should link to {link}");
        }

        for forbidden in ["<link", "<script src", "<img", "@import", "url(", TEST_KEY] {
            assert!(!html.contains(forbidden), "{path} must not contain `{forbidden}`");
        }
    }
}

#[tokio::test]
async fn home_page_signs_in_against_stats_and_opens_the_dashboard() {
    let app = TestApp::start(|_| {}).await;
    let html = page(&app, "/").await;

    assert!(html.contains(r#"<form id="login""#));
    assert!(html.contains(r#"type="password""#));

    for expected in [
        "fetch('/stats'",
        "X-Trout-Key",
        "sessionStorage",
        "location.assign(DASHBOARD)",
    ] {
        assert!(html.contains(expected), "home page should contain `{expected}`");
    }
}

#[tokio::test]
async fn home_page_shows_the_readme_and_how_to_connect() {
    let app = TestApp::start(|_| {}).await;
    let html = page(&app, "/").await;

    for expected in [
        "Connect a client to this relay",
        "X-Trout-Target",
        "/v1/chat/completions",
        r#"href="https://github.com/ankh-intent/trout""#,
        "<h2>Using it</h2>",
        "<h2>Configuration</h2>",
        "<table>",
        r#"href="https://github.com/ankh-intent/trout/blob/master/fly.toml""#,
    ] {
        assert!(html.contains(expected), "home page should contain `{expected}`");
    }

    for forbidden in [r#"href="fly.toml""#, "<!--README-->", "<!--REPO-->"] {
        assert!(
            !html.contains(forbidden),
            "home page must not contain `{forbidden}`"
        );
    }
}

#[tokio::test]
async fn dashboard_sends_the_signed_out_to_the_home_page() {
    let app = TestApp::start(|_| {}).await;
    let html = page(&app, "/dashboard").await;

    assert!(!html.contains(r#"<form"#), "signing in happens on the home page");
    assert!(html.contains("location.replace(HOME)"));
}

#[tokio::test]
async fn providers_page_embeds_its_data() {
    let app = TestApp::start(|_| {}).await;
    let html = page(&app, "/providers").await;
    let marker = r#"<script type="application/json" id="data">"#;
    let start = html.find(marker).expect("embedded data") + marker.len();
    let end = start + html[start..].find("</script>").unwrap();
    let data: serde_json::Value = serde_json::from_str(&html[start..end]).unwrap();

    assert!(!data["providers"].as_array().unwrap().is_empty());
    assert!(data["meta"]["collected"].is_string());
    assert!(data["sources"].is_array());
}

#[tokio::test]
async fn pages_being_public_does_not_open_the_relay() {
    let app = TestApp::start(|_| {}).await;

    for path in ["/stats", "/v1/responses/resp_1", "/v1/chat/completions"] {
        let response = app.client.get(app.url(path)).send().await.unwrap();

        assert_eq!(response.status(), 401, "{path}");
    }
}
