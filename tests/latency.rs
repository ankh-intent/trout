mod common;

use std::time::{Duration, Instant};

use serde_json::json;

use common::{DONE, MockProvider, Reply, TestApp, chat_chunk};

const RUNS: usize = 50;
const BUDGET: Duration = Duration::from_millis(100);

fn stream_reply() -> Reply {
    Reply::Sse {
        chunks: vec![
            chat_chunk(json!({ "content": "first" }), None),
            chat_chunk(json!({ "content": " second" }), Some("stop")),
            DONE.to_string(),
        ],
        gap: Duration::from_millis(5),
    }
}

/// Time from sending the request until the first body bytes that contain `marker` arrive.
async fn first_byte(request: reqwest::RequestBuilder, marker: &str) -> Duration {
    let started = Instant::now();
    let mut response = request.send().await.unwrap();
    let mut seen = String::new();

    assert_eq!(response.status(), 200);

    while !seen.contains(marker) {
        seen.push_str(
            std::str::from_utf8(&response.chunk().await.unwrap().expect("stream ended early")).unwrap(),
        );
    }

    let elapsed = started.elapsed();

    while response.chunk().await.unwrap().is_some() {}

    elapsed
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort_unstable();

    samples[samples.len() / 2]
}

#[tokio::test]
async fn relay_adds_under_100ms_to_the_first_streamed_byte() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let target = mock.target();
    let direct_client = reqwest::Client::builder().no_proxy().build().unwrap();
    let chat_body =
        json!({ "model": "m", "stream": true, "messages": [{ "role": "user", "content": "hi" }] })
            .to_string();
    let (mut direct, mut proxied, mut turns) = (Vec::new(), Vec::new(), Vec::new());

    for _ in 0..RUNS {
        mock.push(stream_reply());
        direct.push(
            first_byte(
                direct_client
                    .post(format!("{target}/chat/completions"))
                    .header("content-type", "application/json")
                    .body(chat_body.clone()),
                "first",
            )
            .await,
        );

        mock.push(stream_reply());
        proxied.push(first_byte(app.chat(&target).body(chat_body.clone()), "first").await);

        mock.push(stream_reply());
        turns.push(
            first_byte(
                app.responses(&target, json!({ "model": "m", "input": "hi", "stream": true })),
                "\"delta\":\"first\"",
            )
            .await,
        );
    }

    let (direct, proxied, turns) = (median(direct), median(proxied), median(turns));

    println!("median first byte: direct {direct:?}, proxied {proxied:?}, responses turn {turns:?}");

    assert!(
        proxied.saturating_sub(direct) < BUDGET,
        "proxy adds {:?}",
        proxied.saturating_sub(direct)
    );
    assert!(
        turns.saturating_sub(direct) < BUDGET,
        "responses turn adds {:?}",
        turns.saturating_sub(direct)
    );
}
