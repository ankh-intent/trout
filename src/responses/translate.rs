use serde_json::{Map, Value, json};

use crate::error::RelayError;
use crate::responses::wire::content_text;
use crate::store::{Completion, Settings, Status, new_id};

/// Builds the body for the target's `/chat/completions` from effective settings and the complete
/// item history (stored history followed by this turn's input).
pub fn to_chat_request(settings: &Settings, items: &[Value], stream: bool) -> Value {
    let mut messages: Vec<Value> = Vec::with_capacity(items.len() + 1);
    // Index of the assistant message that following function calls are folded into.
    let mut open_assistant: Option<usize> = None;

    if let Some(instructions) = settings.get("instructions").and_then(Value::as_str) {
        messages.push(json!({ "role": "system", "content": instructions }));
    }

    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                let role = match item.get("role").and_then(Value::as_str).unwrap_or("user") {
                    "developer" => "system",
                    role => role,
                };

                messages.push(json!({ "role": role, "content": content_text(&item["content"]) }));
                open_assistant = (role == "assistant").then(|| messages.len() - 1);
            }
            Some("function_call") => {
                let call = json!({
                    "id": item["call_id"],
                    "type": "function",
                    "function": { "name": item["name"], "arguments": item["arguments"] },
                });
                let index = *open_assistant.get_or_insert_with(|| {
                    messages.push(json!({ "role": "assistant", "content": null }));

                    messages.len() - 1
                });

                match messages[index]
                    .get_mut("tool_calls")
                    .and_then(Value::as_array_mut)
                {
                    Some(calls) => calls.push(call),
                    None => messages[index]["tool_calls"] = json!([call]),
                }
            }
            Some("function_call_output") => {
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": item["call_id"],
                    "content": item["output"],
                }));
                open_assistant = None;
            }
            _ => {}
        }
    }

    let mut body = Map::new();

    body.insert(
        "model".to_string(),
        settings.get("model").cloned().unwrap_or(Value::Null),
    );
    body.insert("messages".to_string(), Value::Array(messages));

    if let Some(tools) = settings.get("tools").and_then(Value::as_array)
        && !tools.is_empty()
    {
        body.insert(
            "tools".to_string(),
            Value::Array(tools.iter().map(chat_tool).collect()),
        );
    }

    if let Some(choice) = settings.get("tool_choice") {
        let choice = match choice.get("name") {
            Some(name) => json!({ "type": "function", "function": { "name": name } }),
            None => choice.clone(),
        };

        body.insert("tool_choice".to_string(), choice);
    }

    for key in ["parallel_tool_calls", "temperature", "top_p"] {
        if let Some(value) = settings.get(key) {
            body.insert(key.to_string(), value.clone());
        }
    }

    if let Some(max) = settings.get("max_output_tokens") {
        body.insert("max_tokens".to_string(), max.clone());
    }

    if let Some(format) = settings.get("text").and_then(|text| text.get("format")) {
        match format.get("type").and_then(Value::as_str) {
            Some("json_object") => {
                body.insert("response_format".to_string(), json!({ "type": "json_object" }));
            }
            Some("json_schema") => {
                let mut schema = Map::new();

                for key in ["name", "schema", "strict", "description"] {
                    if let Some(value) = format.get(key) {
                        schema.insert(key.to_string(), value.clone());
                    }
                }

                body.insert(
                    "response_format".to_string(),
                    json!({ "type": "json_schema", "json_schema": schema }),
                );
            }
            _ => {}
        }
    }

    if stream {
        body.insert("stream".to_string(), json!(true));
        body.insert("stream_options".to_string(), json!({ "include_usage": true }));
    }

    if let Some(extra) = settings.get("extra_body").and_then(Value::as_object) {
        for (key, value) in extra {
            body.insert(key.clone(), value.clone());
        }
    }

    Value::Object(body)
}

fn chat_tool(tool: &Value) -> Value {
    let mut function = Map::new();

    for key in ["name", "description", "parameters", "strict"] {
        if let Some(value) = tool.get(key) {
            function.insert(key.to_string(), value.clone());
        }
    }

    json!({ "type": "function", "function": function })
}

pub fn reasoning_item(text: &str) -> Value {
    json!({ "id": new_id("rs"), "type": "reasoning", "content": [{ "type": "reasoning_text", "text": text }] })
}

pub fn message_item(text: &str) -> Value {
    json!({
        "id": new_id("msg"),
        "type": "message",
        "role": "assistant",
        "status": "completed",
        "content": [{ "type": "output_text", "text": text }],
    })
}

pub fn function_call_item(call_id: &str, name: &str, arguments: &str) -> Value {
    json!({
        "id": new_id("fc"),
        "type": "function_call",
        "status": "completed",
        "call_id": call_id,
        "name": name,
        "arguments": arguments,
    })
}

pub fn usage(chat_usage: &Value) -> Option<Value> {
    let chat_usage = chat_usage.as_object()?;
    let input = chat_usage
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = chat_usage
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total = chat_usage
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(input + output);
    let mut out = json!({ "input_tokens": input, "output_tokens": output, "total_tokens": total });

    if let Some(cached) = chat_usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
    {
        out["input_tokens_details"] = json!({ "cached_tokens": cached });
    }

    Some(out)
}

/// `finish_reason: "length"` is the only one that leaves the answer unfinished.
pub fn outcome(finish_reason: Option<&str>) -> (Status, Option<String>) {
    match finish_reason {
        Some("length") => (Status::Incomplete, Some("max_output_tokens".to_string())),
        _ => (Status::Completed, None),
    }
}

pub fn reasoning_text(message: &Value) -> Option<&str> {
    ["reasoning_content", "reasoning"]
        .iter()
        .filter_map(|key| message.get(*key).and_then(Value::as_str))
        .find(|text| !text.is_empty())
}

/// Turns a complete (non-streamed) chat completion into the outcome of a turn.
pub fn from_chat_completion(completion: &Value) -> Result<Completion, RelayError> {
    let choice = completion
        .get("choices")
        .and_then(|choices| choices.get(0))
        .ok_or_else(|| RelayError::UpstreamUnreachable("answer is not a chat completion".to_string()))?;
    let message = &choice["message"];
    let mut output_items = Vec::new();

    if let Some(text) = reasoning_text(message) {
        output_items.push(reasoning_item(text));
    }

    if let Some(text) = message
        .get("content")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        output_items.push(message_item(text));
    }

    for call in message
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let call_id = call
            .get("id")
            .and_then(Value::as_str)
            .map_or_else(|| new_id("call"), str::to_string);
        let function = &call["function"];

        output_items.push(function_call_item(
            &call_id,
            function.get("name").and_then(Value::as_str).unwrap_or_default(),
            function.get("arguments").and_then(Value::as_str).unwrap_or("{}"),
        ));
    }

    let (status, incomplete_reason) = outcome(choice.get("finish_reason").and_then(Value::as_str));

    Ok(Completion {
        status: Some(status),
        output_items,
        usage: completion.get("usage").and_then(usage),
        incomplete_reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(value: Value) -> Settings {
        value.as_object().unwrap().clone()
    }

    fn user(text: &str) -> Value {
        json!({ "type": "message", "role": "user", "content": text })
    }

    #[test]
    fn instructions_roles_and_parts() {
        let body = to_chat_request(
            &settings(json!({ "model": "m", "instructions": "Be brief." })),
            &[
                json!({ "type": "message", "role": "developer", "content": "dev note" }),
                json!({ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "a" }, { "type": "input_text", "text": "b" }] }),
                json!({ "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "answer" }] }),
            ],
            false,
        );

        assert_eq!(body["model"], "m");
        assert_eq!(
            body["messages"],
            json!([
                { "role": "system", "content": "Be brief." },
                { "role": "system", "content": "dev note" },
                { "role": "user", "content": "ab" },
                { "role": "assistant", "content": "answer" },
            ])
        );
        assert!(body.get("stream").is_none());
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn assistant_text_and_calls_merge_and_reasoning_is_not_sent() {
        let body = to_chat_request(
            &settings(json!({ "model": "m" })),
            &[
                user("read both"),
                json!({ "type": "reasoning", "content": [{ "type": "reasoning_text", "text": "thinking" }] }),
                json!({ "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "On it." }] }),
                json!({ "type": "function_call", "call_id": "c1", "name": "FileRead", "arguments": "{\"path\":\"a\"}" }),
                json!({ "type": "function_call", "call_id": "c2", "name": "FileRead", "arguments": "{\"path\":\"b\"}" }),
                json!({ "type": "function_call_output", "call_id": "c1", "output": "A" }),
                json!({ "type": "function_call_output", "call_id": "c2", "output": "B" }),
                json!({ "type": "function_call", "call_id": "c3", "name": "Done", "arguments": "{}" }),
            ],
            false,
        );

        assert_eq!(
            body["messages"],
            json!([
                { "role": "user", "content": "read both" },
                { "role": "assistant", "content": "On it.", "tool_calls": [
                    { "id": "c1", "type": "function", "function": { "name": "FileRead", "arguments": "{\"path\":\"a\"}" } },
                    { "id": "c2", "type": "function", "function": { "name": "FileRead", "arguments": "{\"path\":\"b\"}" } },
                ] },
                { "role": "tool", "tool_call_id": "c1", "content": "A" },
                { "role": "tool", "tool_call_id": "c2", "content": "B" },
                { "role": "assistant", "content": null, "tool_calls": [
                    { "id": "c3", "type": "function", "function": { "name": "Done", "arguments": "{}" } },
                ] },
            ])
        );
    }

    #[test]
    fn settings_map_to_chat_fields_and_extra_body_wins() {
        let body = to_chat_request(
            &settings(json!({
                "model": "m",
                "tools": [{ "type": "function", "name": "FileRead", "description": "Read", "parameters": { "type": "object" }, "strict": true }],
                "tool_choice": { "type": "function", "name": "FileRead" },
                "parallel_tool_calls": false,
                "temperature": 0.3,
                "top_p": 0.9,
                "max_output_tokens": 512,
                "text": { "format": { "type": "json_schema", "name": "out", "schema": { "type": "object" }, "strict": true } },
                "extra_body": { "reasoning_split": true, "temperature": 0.7 },
            })),
            &[user("x")],
            true,
        );

        assert_eq!(
            body["tools"],
            json!([{ "type": "function", "function": { "name": "FileRead", "description": "Read", "parameters": { "type": "object" }, "strict": true } }])
        );
        assert_eq!(
            body["tool_choice"],
            json!({ "type": "function", "function": { "name": "FileRead" } })
        );
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(body["top_p"], 0.9);
        assert_eq!(body["max_tokens"], 512);
        assert!(body.get("max_output_tokens").is_none());
        assert_eq!(
            body["response_format"],
            json!({ "type": "json_schema", "json_schema": { "name": "out", "schema": { "type": "object" }, "strict": true } })
        );
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"], json!({ "include_usage": true }));
        assert_eq!(body["reasoning_split"], true);
        assert_eq!(body["temperature"], 0.7);
        assert!(body.get("extra_body").is_none());
    }

    #[test]
    fn simple_tool_choice_and_json_object_format() {
        let body = to_chat_request(
            &settings(
                json!({ "model": "m", "tool_choice": "required", "text": { "format": { "type": "json_object" } } }),
            ),
            &[user("x")],
            false,
        );

        assert_eq!(body["tool_choice"], "required");
        assert_eq!(body["response_format"], json!({ "type": "json_object" }));

        let plain = to_chat_request(
            &settings(json!({ "model": "m", "text": { "format": { "type": "text" } } })),
            &[],
            false,
        );

        assert!(plain.get("response_format").is_none());
    }

    #[test]
    fn completion_becomes_items_in_order() {
        let completion = from_chat_completion(&json!({
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "reasoning_content": "let me look",
                    "content": "Reading.",
                    "tool_calls": [
                        { "id": "call_a", "type": "function", "function": { "name": "FileRead", "arguments": "{\"path\":\"a\"}" } },
                        { "id": "call_b", "type": "function", "function": { "name": "FileRead", "arguments": "{\"path\":\"b\"}" } },
                    ],
                },
            }],
            "usage": { "prompt_tokens": 1200, "completion_tokens": 85, "total_tokens": 1285, "prompt_tokens_details": { "cached_tokens": 1024 } },
        }))
        .unwrap();
        let kinds: Vec<&str> = completion
            .output_items
            .iter()
            .map(|i| i["type"].as_str().unwrap())
            .collect();

        assert_eq!(kinds, ["reasoning", "message", "function_call", "function_call"]);
        assert_eq!(
            completion.output_items[0]["content"][0],
            json!({ "type": "reasoning_text", "text": "let me look" })
        );
        assert_eq!(
            completion.output_items[1]["content"][0],
            json!({ "type": "output_text", "text": "Reading." })
        );
        assert_eq!(completion.output_items[1]["role"], "assistant");
        assert_eq!(completion.output_items[2]["call_id"], "call_a");
        assert_eq!(completion.output_items[3]["arguments"], "{\"path\":\"b\"}");
        assert!(
            completion.output_items[2]["id"]
                .as_str()
                .unwrap()
                .starts_with("fc_")
        );
        assert_eq!(completion.status, Some(Status::Completed));
        assert_eq!(
            completion.usage,
            Some(
                json!({ "input_tokens": 1200, "output_tokens": 85, "total_tokens": 1285, "input_tokens_details": { "cached_tokens": 1024 } })
            )
        );
    }

    #[test]
    fn length_finish_is_incomplete_and_empty_content_is_skipped() {
        let completion = from_chat_completion(&json!({
            "choices": [{ "finish_reason": "length", "message": { "role": "assistant", "content": "" } }],
        }))
        .unwrap();

        assert!(completion.output_items.is_empty());
        assert_eq!(completion.status, Some(Status::Incomplete));
        assert_eq!(completion.incomplete_reason.as_deref(), Some("max_output_tokens"));
        assert_eq!(completion.usage, None);
    }

    #[test]
    fn non_completion_answer_is_an_error() {
        assert!(matches!(
            from_chat_completion(&json!({ "object": "list", "data": [] })),
            Err(RelayError::UpstreamUnreachable(_))
        ));
    }
}
