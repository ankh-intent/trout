mod common;

use serde_json::json;

use common::{MockProvider, Reply, TestApp, completion, completion_with_calls, received};
use trout::codec::gzip;

/// A chat request shaped like a long agent session: many tool results holding source-like text.
fn agent_conversation(target_bytes: usize) -> Vec<u8> {
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;

        seed
    };
    let mut messages = vec![json!({ "role": "system", "content": "You are a coding agent." })];
    let mut size = 0;
    let mut call = 0;

    while size < target_bytes {
        let mut listing = String::new();

        for line in 0..120 {
            let (a, b, c) = (next() % 9973, next() % 613, next() % 97);

            listing.push_str(&format!(
                "{line:>4}\tpub async fn handler_{a}(state: &AppState, request: Request<{b}>) -> Result<Response, RelayError> {{ let value_{c} = state.store.get({a}).await?; Ok(value_{c}.into_response()) }}\n"
            ));
        }

        call += 1;
        size += listing.len();
        messages.push(json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{ "id": format!("call_{call}"), "type": "function", "function": { "name": "FileRead", "arguments": format!("{{\"path\":\"src/module_{}/file_{}.rs\"}}", next() % 40, next() % 400) } }]
        }));
        messages.push(json!({ "role": "tool", "tool_call_id": format!("call_{call}"), "content": listing }));
    }

    serde_json::to_vec(&json!({ "model": "m", "messages": messages })).unwrap()
}

#[tokio::test]
async fn gzip_saves_four_fifths() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let plain = agent_conversation(2 * 1024 * 1024);
    let packed = gzip(&plain).unwrap();

    println!(
        "plain {} bytes, gzip {} bytes, ratio {:.1}x",
        plain.len(),
        packed.len(),
        plain.len() as f64 / packed.len() as f64
    );

    assert!(plain.len() >= 2 * 1024 * 1024);
    assert!(
        packed.len() * 5 <= plain.len(),
        "gzip {} is more than a fifth of {}",
        packed.len(),
        plain.len()
    );

    mock.push(Reply::Json {
        status: 200,
        body: json!({}),
    });
    mock.push(Reply::Json {
        status: 200,
        body: json!({}),
    });

    let via_plain = app.chat(&mock.target()).body(plain.clone()).send().await.unwrap();
    let via_gzip = app
        .chat(&mock.target())
        .header("content-encoding", "gzip")
        .body(packed)
        .send()
        .await
        .unwrap();

    assert_eq!(via_plain.status(), 200);
    assert_eq!(via_gzip.status(), 200);

    let seen = mock.requests();

    assert_eq!(seen[0].body, plain);
    assert_eq!(seen[1].body, plain);
}

fn tool_list(target_bytes: usize) -> serde_json::Value {
    let mut tools = Vec::new();
    let mut size = 0;

    while size < target_bytes {
        let tool = json!({
            "type": "function",
            "name": format!("Tool{}", tools.len()),
            "description": "Reads, writes or searches project files and reports what it found in full detail. ".repeat(4),
            "parameters": { "type": "object", "properties": { "path": { "type": "string", "description": "Path relative to the project root" } }, "required": ["path"] },
        });

        size += tool.to_string().len();
        tools.push(tool);
    }

    json!(tools)
}

#[tokio::test]
async fn follow_up_within_2kb() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let target = mock.target();
    let tools = tool_list(30 * 1024);
    let root = json!({ "model": "m", "instructions": "You are a coding agent. ".repeat(100), "tools": tools, "input": "start" });

    assert!(root.to_string().len() > 30 * 1024);

    mock.push(completion("a0"));

    let mut previous = app.turn(&target, root).await;

    for turn in 1..=50 {
        let new_items =
            json!([{ "role": "user", "content": format!("message {turn}: {}", "detail ".repeat(40)) }]);
        let body = json!({ "previous_response_id": previous["id"], "input": new_items }).to_string();
        let overhead = body.len() - new_items.to_string().len();

        assert!(
            overhead < 2048,
            "turn {turn}: {overhead} bytes beyond the new items"
        );

        mock.push(completion(&format!("a{turn}")));
        previous = app.turn(&target, serde_json::from_str(&body).unwrap()).await;
    }

    let last = received(&mock, 50);

    assert_eq!(last["messages"].as_array().unwrap().len(), 1 + 50 * 2 + 1);
    assert_eq!(
        last["tools"].as_array().unwrap().len(),
        tools.as_array().unwrap().len()
    );
}

#[tokio::test]
async fn fifty_turns_ninety_percent() {
    let mock = MockProvider::start().await;
    let app = TestApp::with_mock(&mock).await;
    let target = mock.target();
    let tools = tool_list(20 * 1024);
    let chat_tools: Vec<serde_json::Value> = tools
        .as_array()
        .unwrap()
        .iter()
        .map(|t| json!({ "type": "function", "function": { "name": t["name"], "description": t["description"], "parameters": t["parameters"] } }))
        .collect();
    let tool_result = |turn: usize| {
        format!(
            "{turn}: {}",
            "fn handler(state: &AppState) -> Result<Response, RelayError>; ".repeat(60)
        )
    };
    let mut via_responses = 0usize;
    let mut via_full_resend = 0usize;
    let mut messages = vec![
        json!({ "role": "system", "content": "You are a coding agent." }),
        json!({ "role": "user", "content": "Refactor the module." }),
    ];
    let root = json!({ "model": "m", "instructions": "You are a coding agent.", "tools": tools, "input": "Refactor the module." }).to_string();

    via_responses += root.len();
    mock.push(completion_with_calls(&[(
        "call_0",
        "Tool0",
        "{\"path\":\"src/a.rs\"}",
    )]));

    let mut previous = app.turn(&target, serde_json::from_str(&root).unwrap()).await;

    for turn in 0..50 {
        // What a client re-submitting everything would upload for this same step.
        via_full_resend += json!({ "model": "m", "messages": messages, "tools": chat_tools })
            .to_string()
            .len();

        let call_id = format!("call_{turn}");

        messages.push(json!({ "role": "assistant", "content": null, "tool_calls": [{ "id": call_id, "type": "function", "function": { "name": "Tool0", "arguments": "{\"path\":\"src/a.rs\"}" } }] }));
        messages.push(json!({ "role": "tool", "tool_call_id": call_id, "content": tool_result(turn) }));

        let body = json!({
            "previous_response_id": previous["id"],
            "input": [{ "type": "function_call_output", "call_id": call_id, "output": tool_result(turn) }],
        })
        .to_string();

        via_responses += body.len();
        mock.push(completion_with_calls(&[(
            &format!("call_{}", turn + 1),
            "Tool0",
            "{\"path\":\"src/a.rs\"}",
        )]));
        previous = app.turn(&target, serde_json::from_str(&body).unwrap()).await;
    }

    let saved = 100.0 * (1.0 - via_responses as f64 / via_full_resend as f64);

    println!(
        "full re-submission {via_full_resend} bytes, stored conversations {via_responses} bytes, saved {saved:.1}%"
    );

    assert!(via_responses * 10 <= via_full_resend, "saved only {saved:.1}%");
    assert_eq!(received(&mock, 50)["messages"], json!(messages));
}
