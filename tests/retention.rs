mod common;

use std::time::Duration;

use axum::http::Method;
use serde_json::json;

use common::{MockProvider, TestApp, completion, received, relay_error_code, remove_store_files};

async fn status(app: &TestApp, id: &serde_json::Value) -> u16 {
    app.admin(Method::GET, &format!("/v1/responses/{}", id.as_str().unwrap()))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

#[tokio::test]
async fn stored_answer_can_be_retrieved() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;

    mock.push(completion("kept"));
    mock.push(completion("second"));

    let first = app
        .turn(
            &mock.target(),
            json!({ "model": "m1", "input": "q", "metadata": { "k": "v" } }),
        )
        .await;
    let second = app
        .turn(
            &mock.target(),
            json!({ "previous_response_id": first["id"], "input": "q2" }),
        )
        .await;
    let response = app
        .admin(
            Method::GET,
            &format!("/v1/responses/{}", second["id"].as_str().unwrap()),
        )
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.headers().get("x-trout-source").unwrap(), "relay");

    let stored: serde_json::Value = response.json().await.unwrap();

    assert_eq!(stored, second);
    assert_eq!(
        app.admin(Method::GET, "/v1/responses/resp_missing")
            .send()
            .await
            .unwrap()
            .status(),
        404
    );

    let stored_first: serde_json::Value = app
        .admin(
            Method::GET,
            &format!("/v1/responses/{}", first["id"].as_str().unwrap()),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(stored_first["metadata"], json!({ "k": "v" }));
}

#[tokio::test]
async fn deleting_a_response_removes_it_and_what_continued_from_it() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let target = mock.target();

    for answer in ["a1", "a2", "a3", "side"] {
        mock.push(completion(answer));
    }

    let first = app.turn(&target, json!({ "model": "m1", "input": "q1" })).await;
    let second = app
        .turn(
            &target,
            json!({ "previous_response_id": first["id"], "input": "q2" }),
        )
        .await;
    let third = app
        .turn(
            &target,
            json!({ "previous_response_id": second["id"], "input": "q3" }),
        )
        .await;
    let side = app
        .turn(
            &target,
            json!({ "previous_response_id": first["id"], "input": "side" }),
        )
        .await;
    let deleted = app
        .admin(
            Method::DELETE,
            &format!("/v1/responses/{}", second["id"].as_str().unwrap()),
        )
        .send()
        .await
        .unwrap();

    assert_eq!(deleted.status(), 200);
    assert_eq!(
        deleted.json::<serde_json::Value>().await.unwrap(),
        json!({ "id": second["id"], "object": "response", "deleted": true })
    );
    assert_eq!(status(&app, &second["id"]).await, 404);
    assert_eq!(status(&app, &third["id"]).await, 404);
    assert_eq!(status(&app, &first["id"]).await, 200);
    assert_eq!(status(&app, &side["id"]).await, 200);

    let follow = app
        .responses(
            &target,
            json!({ "previous_response_id": third["id"], "input": "x" }),
        )
        .send()
        .await
        .unwrap();

    assert_eq!(follow.status(), 404);
    assert_eq!(relay_error_code(follow).await, "response_not_found");

    let again = app
        .admin(
            Method::DELETE,
            &format!("/v1/responses/{}", second["id"].as_str().unwrap()),
        )
        .send()
        .await
        .unwrap();

    assert_eq!(again.status(), 404);
    assert_eq!(relay_error_code(again).await, "response_not_found");
}

#[tokio::test]
async fn deleting_a_conversation_removes_the_whole_tree() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let target = mock.target();

    for answer in ["a1", "a2", "other"] {
        mock.push(completion(answer));
    }

    let first = app.turn(&target, json!({ "model": "m1", "input": "q1" })).await;
    let second = app
        .turn(
            &target,
            json!({ "previous_response_id": first["id"], "input": "q2" }),
        )
        .await;
    let other = app
        .turn(&target, json!({ "model": "m1", "input": "unrelated" }))
        .await;
    let conversation = first["conversation"]["id"].as_str().unwrap();
    let deleted = app
        .admin(Method::DELETE, &format!("/v1/conversations/{conversation}"))
        .send()
        .await
        .unwrap();

    assert_eq!(deleted.status(), 200);
    assert_eq!(
        deleted.json::<serde_json::Value>().await.unwrap(),
        json!({ "id": conversation, "object": "conversation.deleted", "deleted": true })
    );
    assert_eq!(status(&app, &first["id"]).await, 404);
    assert_eq!(status(&app, &second["id"]).await, 404);
    assert_eq!(status(&app, &other["id"]).await, 200);

    let again = app
        .admin(Method::DELETE, &format!("/v1/conversations/{conversation}"))
        .send()
        .await
        .unwrap();

    assert_eq!(again.status(), 404);
}

#[tokio::test]
async fn unused_conversations_expire_and_used_ones_survive() {
    let mock = MockProvider::start().await;
    let entry = mock.entry();
    let app = TestApp::start(move |config| {
        config.targets.list.push(entry);
        config.store.ttl_secs = 2;
        config.store.sweep_interval_secs = 1;
    })
    .await;
    let target = mock.target();

    for answer in ["idle", "busy", "b2", "b3", "b4"] {
        mock.push(completion(answer));
    }

    let idle = app
        .turn(&target, json!({ "model": "m1", "input": "left alone" }))
        .await;
    let mut busy = app
        .turn(&target, json!({ "model": "m1", "input": "kept in use" }))
        .await;

    for _ in 0..3 {
        tokio::time::sleep(Duration::from_millis(1200)).await;
        busy = app
            .turn(
                &target,
                json!({ "previous_response_id": busy["id"], "input": "still here" }),
            )
            .await;
    }

    let expired = app
        .responses(
            &target,
            json!({ "previous_response_id": idle["id"], "input": "too late" }),
        )
        .send()
        .await
        .unwrap();

    assert_eq!(expired.status(), 404);
    assert_eq!(relay_error_code(expired).await, "response_not_found");
    assert_eq!(status(&app, &busy["id"]).await, 200);
}

#[tokio::test]
async fn least_recently_used_conversation_is_evicted_over_the_size_cap() {
    let mock = MockProvider::start().await;
    let entry = mock.entry();
    let app = TestApp::start(move |config| {
        config.targets.list.push(entry);
        config.store.max_bytes = 6_000;
        config.store.sweep_interval_secs = 1;
    })
    .await;
    let target = mock.target();
    let big = "x".repeat(2_500);

    for _ in 0..3 {
        mock.push(completion(&big));
    }

    let oldest = app
        .turn(&target, json!({ "model": "m1", "input": "first" }))
        .await;

    tokio::time::sleep(Duration::from_millis(20)).await;

    let middle = app
        .turn(&target, json!({ "model": "m1", "input": "second" }))
        .await;

    tokio::time::sleep(Duration::from_millis(20)).await;

    let newest = app
        .turn(&target, json!({ "model": "m1", "input": "third" }))
        .await;

    tokio::time::sleep(Duration::from_millis(2500)).await;

    assert_eq!(status(&app, &oldest["id"]).await, 404);
    assert_eq!(status(&app, &middle["id"]).await, 200);
    assert_eq!(status(&app, &newest["id"]).await, 200);
}

#[tokio::test]
async fn conversations_survive_a_restart() {
    let mock = MockProvider::start().await;
    let target = mock.target();
    let app = TestApp::with_mock(&mock).await;

    mock.push(completion("before"));
    mock.push(completion("after"));

    let first = app
        .turn(
            &target,
            json!({ "model": "m1", "instructions": "Stay brief.", "input": "remember" }),
        )
        .await;
    let path = app.stop();
    let entry = mock.entry();
    let restarted = TestApp::start_on(path.clone(), move |config| config.targets.list.push(entry)).await;
    let second = restarted
        .turn(
            &target,
            json!({ "previous_response_id": first["id"], "input": "still there?" }),
        )
        .await;

    assert_eq!(second["conversation"], first["conversation"]);
    assert_eq!(
        received(&mock, 1)["messages"],
        json!([
            { "role": "system", "content": "Stay brief." },
            { "role": "user", "content": "remember" },
            { "role": "assistant", "content": "before" },
            { "role": "user", "content": "still there?" },
        ])
    );

    drop(restarted);
    remove_store_files(&path);
}
