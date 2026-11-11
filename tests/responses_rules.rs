mod common;

use axum::http::Method;
use serde_json::json;

use common::{MockProvider, Reply, TestApp, completion, received, relay_error_code};

#[tokio::test]
async fn unknown_previous_response_is_not_found() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let response = app
        .responses(
            &mock.target(),
            json!({ "previous_response_id": "resp_missing", "input": "x" }),
        )
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 404);
    assert_eq!(relay_error_code(response).await, "response_not_found");
    assert_eq!(mock.request_count(), 0);
}

#[tokio::test]
async fn incomplete_answers_are_retrievable_but_not_continuable() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let target = mock.target();

    mock.push(Reply::Json {
        status: 200,
        body: json!({ "choices": [{ "finish_reason": "length", "message": { "role": "assistant", "content": "cut off mid" } }] }),
    });

    let first = app
        .turn(
            &target,
            json!({ "model": "m1", "max_output_tokens": 4, "input": "long answer please" }),
        )
        .await;

    assert_eq!(first["status"], "incomplete");
    assert_eq!(
        first["incomplete_details"],
        json!({ "reason": "max_output_tokens" })
    );
    assert_eq!(received(&mock, 0)["max_tokens"], 4);

    let id = first["id"].as_str().unwrap();
    let stored: serde_json::Value = app
        .admin(Method::GET, &format!("/v1/responses/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(stored["status"], "incomplete");
    assert_eq!(stored["output"][0]["content"][0]["text"], "cut off mid");

    let follow = app
        .responses(&target, json!({ "previous_response_id": id, "input": "go on" }))
        .send()
        .await
        .unwrap();

    assert_eq!(follow.status(), 409);
    assert_eq!(relay_error_code(follow).await, "response_not_continuable");
    assert_eq!(mock.request_count(), 1);
}

#[tokio::test]
async fn a_conversation_cannot_move_to_another_target() {
    let mock = MockProvider::start().await;
    let other = MockProvider::start().await;
    let (entry, other_entry) = (mock.entry(), other.entry());
    let app = TestApp::start(move |config| {
        config.targets.list.push(entry);
        config.targets.list.push(other_entry);
    })
    .await;

    mock.push(completion("a"));

    let first = app
        .turn(&mock.target(), json!({ "model": "m1", "input": "q" }))
        .await;
    let moved = app
        .responses(
            &other.target(),
            json!({ "previous_response_id": first["id"], "input": "q2" }),
        )
        .send()
        .await
        .unwrap();

    assert_eq!(moved.status(), 409);
    assert_eq!(relay_error_code(moved).await, "target_mismatch");
    assert_eq!(other.request_count(), 0);
}

#[tokio::test]
async fn two_follow_ups_from_one_answer_are_separate_branches() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let target = mock.target();

    for answer in ["root", "left answer", "right answer", "l2", "r2"] {
        mock.push(completion(answer));
    }

    let root = app
        .turn(&target, json!({ "model": "m1", "input": "start" }))
        .await;
    let left = app
        .turn(
            &target,
            json!({ "previous_response_id": root["id"], "input": "go left" }),
        )
        .await;
    let right = app
        .turn(
            &target,
            json!({ "previous_response_id": root["id"], "input": "go right" }),
        )
        .await;

    assert_ne!(left["id"], right["id"]);
    assert_eq!(left["conversation"], right["conversation"]);

    app.turn(
        &target,
        json!({ "previous_response_id": left["id"], "input": "next" }),
    )
    .await;
    app.turn(
        &target,
        json!({ "previous_response_id": right["id"], "input": "next" }),
    )
    .await;

    let contents = |index: usize| -> Vec<String> {
        received(&mock, index)["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["content"].as_str().unwrap().to_string())
            .collect()
    };

    assert_eq!(contents(3), ["start", "root", "go left", "left answer", "next"]);
    assert_eq!(contents(4), ["start", "root", "go right", "right answer", "next"]);
}

#[tokio::test]
async fn store_false_serves_the_turn_and_keeps_nothing() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let target = mock.target();

    mock.push(completion("kept"));
    mock.push(completion("not kept"));

    let unstored_root = {
        mock.push(completion("ephemeral"));

        let stored = app
            .turn(&target, json!({ "model": "m1", "input": "remember this" }))
            .await;
        let unstored = app
            .turn(
                &target,
                json!({ "previous_response_id": stored["id"], "input": "off the record", "store": false }),
            )
            .await;

        assert_eq!(unstored["store"], false);
        assert!(unstored["conversation"].is_null());
        assert_eq!(unstored["output"][0]["content"][0]["text"], "not kept");
        assert_eq!(
            received(&mock, 1)["messages"],
            json!([
                { "role": "user", "content": "remember this" },
                { "role": "assistant", "content": "kept" },
                { "role": "user", "content": "off the record" },
            ])
        );

        let id = unstored["id"].as_str().unwrap().to_string();

        assert_eq!(
            app.admin(Method::GET, &format!("/v1/responses/{id}"))
                .send()
                .await
                .unwrap()
                .status(),
            404
        );

        let follow = app
            .responses(&target, json!({ "previous_response_id": id, "input": "x" }))
            .send()
            .await
            .unwrap();

        assert_eq!(relay_error_code(follow).await, "response_not_found");

        app.turn(
            &target,
            json!({ "model": "m1", "input": "one-off", "store": false }),
        )
        .await
    };

    assert_eq!(unstored_root["status"], "completed");
    assert_eq!(unstored_root["store"], false);
}

#[tokio::test]
async fn provider_error_on_a_turn_is_passed_through_and_nothing_is_stored() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let target = mock.target();
    let provider_error = json!({ "error": { "message": "model overloaded" } });

    mock.push(completion("fine"));
    mock.push(Reply::Json {
        status: 529,
        body: provider_error.clone(),
    });
    mock.push(completion("retried"));

    let first = app.turn(&target, json!({ "model": "m1", "input": "q1" })).await;
    let failed = app
        .responses(
            &target,
            json!({ "previous_response_id": first["id"], "input": "q2" }),
        )
        .send()
        .await
        .unwrap();

    assert_eq!(failed.status(), 529);
    assert_eq!(failed.headers().get("x-trout-source").unwrap(), "upstream");
    assert_eq!(failed.json::<serde_json::Value>().await.unwrap(), provider_error);

    let retried = app
        .turn(
            &target,
            json!({ "previous_response_id": first["id"], "input": "q2" }),
        )
        .await;

    assert_eq!(retried["output"][0]["content"][0]["text"], "retried");
    assert_eq!(
        received(&mock, 2)["messages"],
        json!([
            { "role": "user", "content": "q1" },
            { "role": "assistant", "content": "fine" },
            { "role": "user", "content": "q2" },
        ])
    );
}

#[tokio::test]
async fn answer_that_is_not_a_chat_completion_is_a_relay_error() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;

    mock.push(Reply::Raw {
        status: 200,
        headers: vec![("content-type", "text/html".to_string())],
        body: b"<html>gateway</html>".to_vec(),
    });

    let response = app
        .responses(&mock.target(), json!({ "model": "m1", "input": "q" }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 502);
    assert_eq!(relay_error_code(response).await, "upstream_unreachable");
}
