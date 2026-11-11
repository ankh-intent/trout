mod common;

use serde_json::json;

use common::{
    MockProvider, PROVIDER_KEY, Reply, TestApp, completion, completion_with_calls, gunzip, received,
    relay_error_code,
};

fn tools() -> serde_json::Value {
    json!([{ "type": "function", "name": "FileRead", "description": "Read a file", "parameters": { "type": "object", "properties": { "path": { "type": "string" } } } }])
}

#[tokio::test]
async fn root_turn_returns_a_response_object_with_identifiers() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;

    mock.push(completion("ok"));

    let response = app
        .responses(&mock.target(), json!({ "model": "m1", "instructions": "Answer in one word.", "input": "Say ok.", "metadata": { "run": "7" } }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers().get("x-trout-source").unwrap(), "relay");
    assert!(response.headers().contains_key("x-trout-request-id"));

    let body: serde_json::Value = response.json().await.unwrap();

    assert!(body["id"].as_str().unwrap().starts_with("resp_"));
    assert!(body["conversation"]["id"].as_str().unwrap().starts_with("conv_"));
    assert_eq!(body["object"], "response");
    assert_eq!(body["status"], "completed");
    assert_eq!(body["model"], "m1");
    assert_eq!(body["store"], true);
    assert_eq!(body["metadata"], json!({ "run": "7" }));
    assert!(body["previous_response_id"].is_null());
    assert!(body["incomplete_details"].is_null());
    assert_eq!(body["output"].as_array().unwrap().len(), 1);
    assert_eq!(body["output"][0]["type"], "message");
    assert_eq!(body["output"][0]["role"], "assistant");
    assert_eq!(
        body["output"][0]["content"],
        json!([{ "type": "output_text", "text": "ok" }])
    );
    assert_eq!(
        body["usage"],
        json!({ "input_tokens": 10, "output_tokens": 3, "total_tokens": 13 })
    );

    let seen = mock.requests();

    assert_eq!(seen[0].path_and_query, "/v1/chat/completions");
    assert_eq!(seen[0].headers.get("authorization").unwrap(), PROVIDER_KEY);
    assert_eq!(seen[0].headers.get("content-type").unwrap(), "application/json");
    assert!(
        seen[0]
            .headers
            .keys()
            .all(|name| !name.as_str().starts_with("x-trout-"))
    );
    assert_eq!(
        received(&mock, 0),
        json!({
            "model": "m1",
            "messages": [
                { "role": "system", "content": "Answer in one word." },
                { "role": "user", "content": "Say ok." },
            ],
        })
    );
}

#[tokio::test]
async fn follow_up_sends_only_new_input_and_the_provider_gets_everything() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let target = mock.target();

    mock.push(completion("Noted."));
    mock.push(completion("Ada."));

    let first = app
        .turn(&target, json!({ "model": "m1", "instructions": "Be brief.", "tools": tools(), "temperature": 0.2, "input": "My name is Ada." }))
        .await;
    let second = app
        .turn(
            &target,
            json!({ "previous_response_id": first["id"], "input": "What is my name?" }),
        )
        .await;

    assert_eq!(second["previous_response_id"], first["id"]);
    assert_eq!(second["conversation"], first["conversation"]);
    assert_eq!(second["model"], "m1");
    assert_eq!(second["output"][0]["content"][0]["text"], "Ada.");

    let upstream = received(&mock, 1);

    assert_eq!(upstream["model"], "m1");
    assert_eq!(upstream["temperature"], 0.2);
    assert_eq!(upstream["tools"][0]["function"]["name"], "FileRead");
    assert_eq!(
        upstream["messages"],
        json!([
            { "role": "system", "content": "Be brief." },
            { "role": "user", "content": "My name is Ada." },
            { "role": "assistant", "content": "Noted." },
            { "role": "user", "content": "What is my name?" },
        ])
    );
}

#[tokio::test]
async fn overrides_replace_and_persist_and_null_clears() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let target = mock.target();

    for _ in 0..3 {
        mock.push(completion("ok"));
    }

    let first = app
        .turn(
            &target,
            json!({ "model": "m1", "instructions": "A", "tools": tools(), "input": "1" }),
        )
        .await;
    let second = app
        .turn(&target, json!({ "previous_response_id": first["id"], "model": "m2", "instructions": "B", "tools": null, "input": "2" }))
        .await;
    let third = app
        .turn(
            &target,
            json!({ "previous_response_id": second["id"], "input": "3" }),
        )
        .await;

    assert_eq!(second["model"], "m2");
    assert_eq!(third["model"], "m2");

    for index in [1, 2] {
        let upstream = received(&mock, index);

        assert_eq!(upstream["model"], "m2");
        assert_eq!(
            upstream["messages"][0],
            json!({ "role": "system", "content": "B" })
        );
        assert!(upstream.get("tools").is_none());
    }
}

#[tokio::test]
async fn tool_calls_and_their_results_are_paired_for_the_provider() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let target = mock.target();

    mock.push(completion_with_calls(&[
        ("call_a", "FileRead", "{\"path\":\"a.rs\"}"),
        ("call_b", "FileRead", "{\"path\":\"b.rs\"}"),
    ]));
    mock.push(completion("Both read."));

    let first = app
        .turn(
            &target,
            json!({ "model": "m1", "tools": tools(), "input": "Read a.rs and b.rs" }),
        )
        .await;
    let calls = first["output"].as_array().unwrap();

    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["type"], "function_call");
    assert_eq!(calls[0]["call_id"], "call_a");
    assert_eq!(calls[0]["name"], "FileRead");
    assert_eq!(calls[1]["arguments"], "{\"path\":\"b.rs\"}");

    let second = app
        .turn(
            &target,
            json!({
                "previous_response_id": first["id"],
                "input": [
                    { "type": "function_call_output", "call_id": "call_a", "output": "fn a() {}" },
                    { "type": "function_call_output", "call_id": "call_b", "output": "fn b() {}" },
                ],
            }),
        )
        .await;

    assert_eq!(second["output"][0]["content"][0]["text"], "Both read.");
    assert_eq!(
        received(&mock, 1)["messages"],
        json!([
            { "role": "user", "content": "Read a.rs and b.rs" },
            { "role": "assistant", "content": null, "tool_calls": [
                { "id": "call_a", "type": "function", "function": { "name": "FileRead", "arguments": "{\"path\":\"a.rs\"}" } },
                { "id": "call_b", "type": "function", "function": { "name": "FileRead", "arguments": "{\"path\":\"b.rs\"}" } },
            ] },
            { "role": "tool", "tool_call_id": "call_a", "content": "fn a() {}" },
            { "role": "tool", "tool_call_id": "call_b", "content": "fn b() {}" },
        ])
    );
}

#[tokio::test]
async fn reasoning_is_returned_and_kept_but_never_sent_back() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let target = mock.target();

    mock.push(Reply::Json {
        status: 200,
        body: json!({ "choices": [{ "finish_reason": "stop", "message": { "role": "assistant", "reasoning_content": "private thoughts", "content": "Answer." } }] }),
    });
    mock.push(completion("More."));

    let first = app.turn(&target, json!({ "model": "m1", "input": "q" })).await;

    assert_eq!(first["output"][0]["type"], "reasoning");
    assert_eq!(
        first["output"][0]["content"][0],
        json!({ "type": "reasoning_text", "text": "private thoughts" })
    );
    assert_eq!(first["output"][1]["type"], "message");

    app.turn(
        &target,
        json!({ "previous_response_id": first["id"], "input": "again" }),
    )
    .await;

    assert!(
        !mock.requests()[1]
            .body
            .windows(16)
            .any(|w| w == b"private thoughts")
    );
}

#[tokio::test]
async fn request_and_answer_can_both_be_gzip() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let body = json!({ "model": "m1", "input": "compressed question" }).to_string();

    mock.push(completion("compressed answer"));

    let response = app
        .request(axum::http::Method::POST, "/v1/responses", &mock.target())
        .header("content-type", "application/json")
        .header("content-encoding", "gzip")
        .header("accept-encoding", "gzip")
        .body(trout::codec::gzip(body.as_bytes()).unwrap())
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers().get("content-encoding").unwrap(), "gzip");

    let object: serde_json::Value =
        serde_json::from_slice(&gunzip(&response.bytes().await.unwrap())).unwrap();

    assert_eq!(object["output"][0]["content"][0]["text"], "compressed answer");
    assert_eq!(
        received(&mock, 0)["messages"][0]["content"],
        "compressed question"
    );
}

#[tokio::test]
async fn invalid_and_unsupported_requests_never_reach_the_provider() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let target = mock.target();
    let cases = [
        (json!({ "input": "no model" }), 400, "invalid_request"),
        (json!({ "model": "m1" }), 400, "invalid_request"),
        (
            json!({ "model": "m1", "input": [{ "type": "function_call_output", "call_id": "nope", "output": "x" }] }),
            400,
            "invalid_request",
        ),
        (
            json!({ "model": "m1", "input": [{ "role": "user", "content": [{ "type": "input_image", "image_url": "x" }] }] }),
            400,
            "unsupported_feature",
        ),
        (
            json!({ "model": "m1", "input": "x", "tools": [{ "type": "web_search" }] }),
            400,
            "unsupported_feature",
        ),
        (
            json!({ "model": "m1", "input": "x", "background": true }),
            400,
            "unsupported_feature",
        ),
    ];

    for (body, status, code) in cases {
        let response = app.responses(&target, body.clone()).send().await.unwrap();

        assert_eq!(response.status(), status, "{body}");
        assert_eq!(relay_error_code(response).await, code, "{body}");
    }

    let no_target = app
        .admin(axum::http::Method::POST, "/v1/responses")
        .body(json!({ "model": "m", "input": "x" }).to_string())
        .send()
        .await
        .unwrap();

    assert_eq!(relay_error_code(no_target).await, "target_missing");
    assert_eq!(mock.request_count(), 0);
}
