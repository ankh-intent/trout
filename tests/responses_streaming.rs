mod common;

use std::io::Write;
use std::time::Duration;

use axum::http::Method;
use serde_json::json;

use common::{
    DONE, MockProvider, Reply, TestApp, chat_chunk, completion, received, relay_error_code, sse_events,
    usage_chunk,
};

const NO_GAP: Duration = Duration::from_millis(1);

fn kinds(events: &[(String, serde_json::Value)]) -> Vec<&str> {
    events.iter().map(|(name, _)| name.as_str()).collect()
}

#[tokio::test]
async fn text_answer_event_sequence() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let target = mock.target();

    mock.push(Reply::Sse {
        chunks: vec![
            chat_chunk(json!({ "role": "assistant", "content": "" }), None),
            chat_chunk(json!({ "content": "Hel" }), None),
            chat_chunk(json!({ "content": "lo" }), None),
            chat_chunk(json!({}), Some("stop")),
            usage_chunk(),
            DONE.to_string(),
        ],
        gap: NO_GAP,
    });
    mock.push(completion("again"));

    let response = app
        .responses(&target, json!({ "model": "m1", "input": "hi", "stream": true }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    assert_eq!(response.headers().get("x-trout-source").unwrap(), "relay");

    let events = sse_events(&response.text().await.unwrap());

    assert_eq!(
        kinds(&events),
        [
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.delta",
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
            "response.completed",
        ]
    );

    for (index, (name, data)) in events.iter().enumerate() {
        assert_eq!(data["type"], name.as_str());
        assert_eq!(data["sequence_number"], index);
    }

    let created = &events[0].1["response"];
    let completed = &events.last().unwrap().1["response"];

    assert_eq!(created["status"], "in_progress");
    assert_eq!(created["output"], json!([]));
    assert_eq!(completed["id"], created["id"]);
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["output"][0]["content"][0]["text"], "Hello");
    assert_eq!(
        completed["usage"],
        json!({ "input_tokens": 10, "output_tokens": 3, "total_tokens": 13 })
    );
    assert!(
        completed["conversation"]["id"]
            .as_str()
            .unwrap()
            .starts_with("conv_")
    );

    let upstream = received(&mock, 0);

    assert_eq!(upstream["stream"], true);
    assert_eq!(upstream["stream_options"], json!({ "include_usage": true }));

    let next = app
        .turn(
            &target,
            json!({ "previous_response_id": completed["id"], "input": "more" }),
        )
        .await;

    assert_eq!(next["status"], "completed");
    assert_eq!(
        received(&mock, 1)["messages"],
        json!([
            { "role": "user", "content": "hi" },
            { "role": "assistant", "content": "Hello" },
            { "role": "user", "content": "more" },
        ])
    );
}

#[tokio::test]
async fn reasoning_and_tool_call_events() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;

    mock.push(Reply::Sse {
        chunks: vec![
            chat_chunk(json!({ "reasoning_content": "hmm" }), None),
            chat_chunk(json!({ "content": "Reading." }), None),
            chat_chunk(json!({ "tool_calls": [{ "index": 0, "id": "call_a", "type": "function", "function": { "name": "FileRead", "arguments": "{\"pa" } }] }), None),
            chat_chunk(json!({ "tool_calls": [{ "index": 0, "function": { "arguments": "th\":\"a\"}" } }] }), None),
            chat_chunk(json!({}), Some("tool_calls")),
            DONE.to_string(),
        ],
        gap: NO_GAP,
    });

    let response = app
        .responses(
            &mock.target(),
            json!({ "model": "m1", "input": "go", "stream": true }),
        )
        .send()
        .await
        .unwrap();
    let events = sse_events(&response.text().await.unwrap());
    let names = kinds(&events);

    for expected in [
        "response.reasoning_text.delta",
        "response.reasoning_text.done",
        "response.output_text.delta",
        "response.function_call_arguments.delta",
        "response.function_call_arguments.done",
    ] {
        assert!(names.contains(&expected), "missing {expected} in {names:?}");
    }

    let output = events.last().unwrap().1["response"]["output"]
        .as_array()
        .unwrap()
        .clone();
    let types: Vec<&str> = output.iter().map(|item| item["type"].as_str().unwrap()).collect();

    assert_eq!(types, ["reasoning", "message", "function_call"]);
    assert_eq!(output[2]["call_id"], "call_a");
    assert_eq!(output[2]["arguments"], "{\"path\":\"a\"}");

    let done = events
        .iter()
        .find(|(name, _)| name == "response.function_call_arguments.done")
        .unwrap();

    assert_eq!(done.1["arguments"], "{\"path\":\"a\"}");
    assert_eq!(done.1["item_id"], output[2]["id"]);
}

#[tokio::test]
async fn id_is_not_continuable_until_the_stream_completes() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let target = mock.target();

    mock.push(Reply::Sse {
        chunks: vec![
            chat_chunk(json!({ "content": "slow" }), None),
            chat_chunk(json!({}), Some("stop")),
            DONE.to_string(),
        ],
        gap: Duration::from_millis(400),
    });
    mock.push(completion("after"));

    let mut response = app
        .responses(&target, json!({ "model": "m1", "input": "hi", "stream": true }))
        .send()
        .await
        .unwrap();
    let mut text = String::new();

    while !text.contains("response.in_progress") {
        text.push_str(std::str::from_utf8(&response.chunk().await.unwrap().unwrap()).unwrap());
    }

    let id = sse_events(&text)[0].1["response"]["id"].clone();
    let early = app
        .responses(
            &target,
            json!({ "previous_response_id": id, "input": "too soon" }),
        )
        .send()
        .await
        .unwrap();

    assert_eq!(early.status(), 409);
    assert_eq!(relay_error_code(early).await, "response_in_progress");

    let running = app
        .admin(Method::GET, &format!("/v1/responses/{}", id.as_str().unwrap()))
        .send()
        .await
        .unwrap();
    let running: serde_json::Value = running.json().await.unwrap();

    assert_eq!(running["status"], "in_progress");
    assert_eq!(running["output"], json!([]));

    let blocked = app
        .admin(Method::DELETE, &format!("/v1/responses/{}", id.as_str().unwrap()))
        .send()
        .await
        .unwrap();

    assert_eq!(blocked.status(), 409);

    while let Some(chunk) = response.chunk().await.unwrap() {
        text.push_str(std::str::from_utf8(&chunk).unwrap());
    }

    assert_eq!(sse_events(&text).last().unwrap().0, "response.completed");

    let after = app
        .turn(&target, json!({ "previous_response_id": id, "input": "now" }))
        .await;

    assert_eq!(after["status"], "completed");
}

#[tokio::test]
async fn upstream_failure_mid_stream_emits_error_and_stores_nothing() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let target = mock.target();

    // The provider's stream ends without ever reporting a finish reason.
    mock.push(Reply::Sse {
        chunks: vec![chat_chunk(json!({ "content": "half an ans" }), None)],
        gap: NO_GAP,
    });

    let response = app
        .responses(&target, json!({ "model": "m1", "input": "hi", "stream": true }))
        .send()
        .await
        .unwrap();
    let events = sse_events(&response.text().await.unwrap());
    let (last_name, last) = events.last().unwrap();

    assert_eq!(last_name, "error");
    assert_eq!(last["error"]["type"], "trout_error");
    assert_eq!(last["error"]["code"], "upstream_unreachable");
    assert!(!kinds(&events).contains(&"response.completed"));

    let id = events[0].1["response"]["id"].as_str().unwrap().to_string();
    let gone = app
        .admin(Method::GET, &format!("/v1/responses/{id}"))
        .send()
        .await
        .unwrap();

    assert_eq!(gone.status(), 404);

    let follow = app
        .responses(&target, json!({ "previous_response_id": id, "input": "x" }))
        .send()
        .await
        .unwrap();

    assert_eq!(relay_error_code(follow).await, "response_not_found");
}

#[tokio::test]
async fn client_disconnect_mid_stream_stores_nothing_and_stops_the_provider() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let chunks: Vec<String> = (0..100)
        .map(|i| chat_chunk(json!({ "content": format!("w{i} ") }), None))
        .collect();

    mock.push(Reply::Sse {
        chunks,
        gap: Duration::from_millis(30),
    });

    let mut response = app
        .responses(
            &mock.target(),
            json!({ "model": "m1", "input": "hi", "stream": true }),
        )
        .send()
        .await
        .unwrap();
    let mut text = String::new();

    while !text.contains("response.output_text.delta") {
        text.push_str(std::str::from_utf8(&response.chunk().await.unwrap().unwrap()).unwrap());
    }

    let id = sse_events(&text)[0].1["response"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    drop(response);

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut status = 0;

    while std::time::Instant::now() < deadline {
        status = app
            .admin(Method::GET, &format!("/v1/responses/{id}"))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16();

        if status == 404 && mock.stream_dropped_early() {
            break;
        }

        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    assert_eq!(status, 404, "abandoned turn is still stored");
    assert!(
        mock.stream_dropped_early(),
        "provider stream kept running after the client left"
    );
}

#[tokio::test]
async fn compressed_stream_is_decodable_event_by_event() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let gap = Duration::from_millis(200);

    mock.push(Reply::Sse {
        chunks: vec![
            chat_chunk(json!({ "content": "one" }), None),
            chat_chunk(json!({ "content": "two" }), None),
            chat_chunk(json!({ "content": "three" }), Some("stop")),
            DONE.to_string(),
        ],
        gap,
    });

    let started = std::time::Instant::now();
    let mut response = app
        .responses(
            &mock.target(),
            json!({ "model": "m1", "input": "hi", "stream": true }),
        )
        .header("accept-encoding", "gzip")
        .send()
        .await
        .unwrap();

    assert_eq!(response.headers().get("content-encoding").unwrap(), "gzip");

    let mut decoder = flate2::write::GzDecoder::new(Vec::new());
    let mut first_delta_at = None;

    while let Some(chunk) = response.chunk().await.unwrap() {
        decoder.write_all(&chunk).unwrap();
        decoder.flush().unwrap();

        if first_delta_at.is_none()
            && String::from_utf8_lossy(decoder.get_ref()).contains("\"delta\":\"one\"")
        {
            first_delta_at = Some(started.elapsed());
        }
    }

    let total = started.elapsed();
    let text = String::from_utf8(decoder.finish().unwrap()).unwrap();

    assert_eq!(sse_events(&text).last().unwrap().0, "response.completed");
    assert!(
        first_delta_at.unwrap() < gap * 2,
        "first delta decodable only after {first_delta_at:?}"
    );
    assert!(total >= gap * 3);
}
