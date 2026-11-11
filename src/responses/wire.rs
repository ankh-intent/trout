use serde_json::{Map, Value, json};

use crate::error::RelayError;
use crate::store::{Settings, Status};

/// Request fields a follow-up inherits from the answer it continues.
pub const INHERITABLE: [&str; 10] = [
    "model",
    "instructions",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "temperature",
    "top_p",
    "max_output_tokens",
    "text",
    "extra_body",
];

const OTHER_FIELDS: [&str; 5] = ["input", "previous_response_id", "stream", "store", "metadata"];

#[derive(Debug, Clone)]
pub struct ResponsesRequest {
    pub previous_response_id: Option<String>,
    /// Validated input items, with `reasoning` items dropped.
    pub input: Vec<Value>,
    /// Inheritable fields present in the body; `null` clears an inherited value.
    pub overrides: Settings,
    pub stream: bool,
    pub store: bool,
    pub metadata: Option<Value>,
}

fn invalid(message: impl Into<String>) -> RelayError {
    RelayError::InvalidRequest(message.into())
}

fn unsupported(message: impl Into<String>) -> RelayError {
    RelayError::UnsupportedFeature(message.into())
}

pub fn parse_request(body: &[u8]) -> Result<ResponsesRequest, RelayError> {
    let value: Value = serde_json::from_slice(body).map_err(|_| invalid("body is not valid JSON"))?;
    let Value::Object(mut body) = value else {
        return Err(invalid("body must be a JSON object"));
    };

    if let Some(unknown) = body
        .keys()
        .find(|k| !INHERITABLE.contains(&k.as_str()) && !OTHER_FIELDS.contains(&k.as_str()))
    {
        return Err(unsupported(format!("field `{unknown}`")));
    }

    let input = parse_input(
        body.remove("input")
            .ok_or_else(|| invalid("`input` is required"))?,
    )?;
    let previous_response_id = optional_string(&mut body, "previous_response_id")?;
    let stream = optional_bool(&mut body, "stream")?.unwrap_or(false);
    let store = optional_bool(&mut body, "store")?.unwrap_or(true);
    let metadata = match body.remove("metadata") {
        None | Some(Value::Null) => None,
        Some(value @ Value::Object(_)) => Some(value),
        Some(_) => return Err(invalid("`metadata` must be an object")),
    };
    let mut overrides = Map::new();

    for key in INHERITABLE {
        if let Some(value) = body.remove(key) {
            if !value.is_null() {
                check_setting(key, &value)?;
            }

            overrides.insert(key.to_string(), value);
        }
    }

    Ok(ResponsesRequest {
        previous_response_id,
        input,
        overrides,
        stream,
        store,
        metadata,
    })
}

fn optional_string(body: &mut Map<String, Value>, key: &str) -> Result<Option<String>, RelayError> {
    match body.remove(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(invalid(format!("`{key}` must be a string"))),
    }
}

fn optional_bool(body: &mut Map<String, Value>, key: &str) -> Result<Option<bool>, RelayError> {
    match body.remove(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(value)),
        Some(_) => Err(invalid(format!("`{key}` must be a boolean"))),
    }
}

fn check_setting(key: &str, value: &Value) -> Result<(), RelayError> {
    match key {
        "model" | "instructions" if !value.is_string() => Err(invalid(format!("`{key}` must be a string"))),
        "parallel_tool_calls" if !value.is_boolean() => {
            Err(invalid("`parallel_tool_calls` must be a boolean"))
        }
        "temperature" | "top_p" if !value.is_number() => Err(invalid(format!("`{key}` must be a number"))),
        "max_output_tokens" if !value.is_u64() => {
            Err(invalid("`max_output_tokens` must be a positive integer"))
        }
        "extra_body" if !value.is_object() => Err(invalid("`extra_body` must be an object")),
        "tools" => check_tools(value),
        "tool_choice" => check_tool_choice(value),
        "text" => check_text(value),
        _ => Ok(()),
    }
}

fn check_tools(value: &Value) -> Result<(), RelayError> {
    let tools = value
        .as_array()
        .ok_or_else(|| invalid("`tools` must be an array"))?;

    for tool in tools {
        let kind = tool.get("type").and_then(Value::as_str).unwrap_or_default();

        if kind != "function" {
            return Err(unsupported(format!("tool type `{kind}`")));
        }

        if !tool.get("name").is_some_and(Value::is_string) {
            return Err(invalid("every function tool needs a `name`"));
        }
    }

    Ok(())
}

fn check_tool_choice(value: &Value) -> Result<(), RelayError> {
    match value {
        Value::String(choice) if matches!(choice.as_str(), "auto" | "none" | "required") => Ok(()),
        Value::Object(choice)
            if choice.get("type").and_then(Value::as_str) == Some("function")
                && choice.get("name").is_some_and(Value::is_string) =>
        {
            Ok(())
        }
        _ => Err(unsupported("this `tool_choice` form")),
    }
}

fn check_text(value: &Value) -> Result<(), RelayError> {
    let Some(format) = value.get("format") else {
        return Ok(());
    };

    match format.get("type").and_then(Value::as_str) {
        Some("text" | "json_object") => Ok(()),
        Some("json_schema")
            if format.get("name").is_some_and(Value::is_string) && format.get("schema").is_some() =>
        {
            Ok(())
        }
        Some("json_schema") => Err(invalid("`text.format` json_schema needs `name` and `schema`")),
        other => Err(unsupported(format!(
            "text format `{}`",
            other.unwrap_or_default()
        ))),
    }
}

fn parse_input(input: Value) -> Result<Vec<Value>, RelayError> {
    match input {
        Value::String(text) => Ok(vec![
            json!({ "type": "message", "role": "user", "content": text }),
        ]),
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());

            for item in items {
                if let Some(item) = parse_item(item)? {
                    out.push(item);
                }
            }

            Ok(out)
        }
        _ => Err(invalid("`input` must be a string or an array of items")),
    }
}

/// Validates one input item. `None` means the item is accepted but not kept (reasoning).
fn parse_item(item: Value) -> Result<Option<Value>, RelayError> {
    let Value::Object(mut item) = item else {
        return Err(invalid("every input item must be an object"));
    };
    let kind = match item.get("type").and_then(Value::as_str) {
        Some(kind) => kind.to_string(),
        None if item.contains_key("role") => {
            item.insert("type".to_string(), json!("message"));

            "message".to_string()
        }
        None => return Err(invalid("input item has neither `type` nor `role`")),
    };
    let has_string = |item: &Map<String, Value>, key: &str| item.get(key).is_some_and(Value::is_string);

    match kind.as_str() {
        "message" => {
            let role = item.get("role").and_then(Value::as_str).unwrap_or_default();

            if !matches!(role, "user" | "system" | "developer" | "assistant") {
                return Err(invalid(format!("message role `{role}`")));
            }

            check_content(item.get("content"))?;
        }
        "function_call" => {
            if !(has_string(&item, "call_id") && has_string(&item, "name") && has_string(&item, "arguments"))
            {
                return Err(invalid(
                    "function_call needs string `call_id`, `name` and `arguments`",
                ));
            }
        }
        "function_call_output" => {
            if !(has_string(&item, "call_id") && has_string(&item, "output")) {
                return Err(invalid(
                    "function_call_output needs string `call_id` and `output`",
                ));
            }
        }
        "reasoning" => return Ok(None),
        other => return Err(unsupported(format!("input item type `{other}`"))),
    }

    Ok(Some(Value::Object(item)))
}

fn check_content(content: Option<&Value>) -> Result<(), RelayError> {
    match content {
        Some(Value::String(_)) => Ok(()),
        Some(Value::Array(parts)) => {
            for part in parts {
                let kind = part.get("type").and_then(Value::as_str).unwrap_or_default();

                if !matches!(kind, "input_text" | "output_text" | "text") {
                    return Err(unsupported(format!("content part type `{kind}`")));
                }

                if !part.get("text").is_some_and(Value::is_string) {
                    return Err(invalid("text content part needs a string `text`"));
                }
            }

            Ok(())
        }
        _ => Err(invalid("message `content` must be a string or an array of parts")),
    }
}

/// The text of a message item, whatever form its content takes.
pub fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .concat(),
        _ => String::new(),
    }
}

/// The fixed part of a response object; status, output and usage are filled in per use.
#[derive(Debug, Clone)]
pub struct Envelope {
    pub id: String,
    pub created_at_ms: i64,
    pub model: String,
    pub previous_response_id: Option<String>,
    pub conversation_id: Option<String>,
    pub metadata: Option<Value>,
}

impl Envelope {
    pub fn object(
        &self,
        status: Status,
        output: &[Value],
        usage: Option<&Value>,
        incomplete_reason: Option<&str>,
    ) -> Value {
        json!({
            "id": self.id,
            "object": "response",
            "created_at": self.created_at_ms / 1000,
            "status": status.as_str(),
            "model": self.model,
            "previous_response_id": self.previous_response_id,
            "conversation": self.conversation_id.as_ref().map(|id| json!({ "id": id })),
            "output": output,
            "usage": usage,
            "incomplete_details": incomplete_reason.map(|reason| json!({ "reason": reason })),
            "metadata": self.metadata.clone().unwrap_or_else(|| json!({})),
            "store": self.conversation_id.is_some(),
        })
    }
}

pub fn deleted_response(id: &str) -> Value {
    json!({ "id": id, "object": "response", "deleted": true })
}

pub fn deleted_conversation(id: &str) -> Value {
    json!({ "id": id, "object": "conversation.deleted", "deleted": true })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: Value) -> Result<ResponsesRequest, RelayError> {
        parse_request(body.to_string().as_bytes())
    }

    fn code(body: Value) -> &'static str {
        parse(body).unwrap_err().code()
    }

    #[test]
    fn string_input_becomes_a_user_message_and_defaults_apply() {
        let request = parse(json!({ "model": "m", "input": "hi" })).unwrap();

        assert_eq!(
            request.input,
            vec![json!({ "type": "message", "role": "user", "content": "hi" })]
        );
        assert!(!request.stream);
        assert!(request.store);
        assert_eq!(request.previous_response_id, None);
        assert_eq!(
            request.overrides,
            json!({ "model": "m" }).as_object().unwrap().clone()
        );
    }

    #[test]
    fn only_present_settings_become_overrides_and_null_is_kept() {
        let request = parse(json!({
            "previous_response_id": "resp_1",
            "input": [{ "role": "user", "content": "x" }],
            "tools": null,
            "temperature": 0.5,
            "stream": true,
            "store": false,
            "metadata": { "k": "v" }
        }))
        .unwrap();

        assert_eq!(
            request.overrides,
            json!({ "tools": null, "temperature": 0.5 })
                .as_object()
                .unwrap()
                .clone()
        );
        assert_eq!(request.input[0]["type"], "message");
        assert!(request.stream);
        assert!(!request.store);
        assert_eq!(request.metadata, Some(json!({ "k": "v" })));
    }

    #[test]
    fn reasoning_items_are_accepted_and_dropped() {
        let request = parse(json!({
            "model": "m",
            "input": [
                { "type": "reasoning", "content": [] },
                { "type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}" },
                { "type": "function_call_output", "call_id": "c1", "output": "ok" }
            ]
        }))
        .unwrap();

        assert_eq!(request.input.len(), 2);
    }

    #[test]
    fn rejections() {
        assert_eq!(parse_request(b"not json").unwrap_err().code(), "invalid_request");
        assert_eq!(code(json!({ "model": "m" })), "invalid_request");
        assert_eq!(code(json!({ "model": "m", "input": 5 })), "invalid_request");
        assert_eq!(code(json!({ "model": 5, "input": "x" })), "invalid_request");
        assert_eq!(
            code(json!({ "model": "m", "input": "x", "include": [] })),
            "unsupported_feature"
        );
        assert_eq!(
            code(json!({ "model": "m", "input": [{ "role": "tool", "content": "x" }] })),
            "invalid_request"
        );
        assert_eq!(
            code(json!({ "model": "m", "input": [{ "type": "image_generation_call" }] })),
            "unsupported_feature"
        );
        assert_eq!(
            code(
                json!({ "model": "m", "input": [{ "role": "user", "content": [{ "type": "input_image", "image_url": "x" }] }] })
            ),
            "unsupported_feature"
        );
        assert_eq!(
            code(json!({ "model": "m", "input": "x", "tools": [{ "type": "web_search" }] })),
            "unsupported_feature"
        );
        assert_eq!(
            code(json!({ "model": "m", "input": "x", "tools": [{ "type": "function" }] })),
            "invalid_request"
        );
        assert_eq!(
            code(json!({ "model": "m", "input": "x", "tool_choice": { "type": "web_search" } })),
            "unsupported_feature"
        );
        assert_eq!(
            code(json!({ "model": "m", "input": "x", "text": { "format": { "type": "xml" } } })),
            "unsupported_feature"
        );
        assert_eq!(
            code(
                json!({ "model": "m", "input": [{ "type": "function_call_output", "call_id": "c", "output": [] }] })
            ),
            "invalid_request"
        );
    }

    #[test]
    fn content_text_joins_parts() {
        assert_eq!(content_text(&json!("plain")), "plain");
        assert_eq!(
            content_text(
                &json!([{ "type": "input_text", "text": "a" }, { "type": "output_text", "text": "b" }])
            ),
            "ab"
        );
    }

    #[test]
    fn envelope_object_shape() {
        let envelope = Envelope {
            id: "resp_1".to_string(),
            created_at_ms: 1_794_300_000_500,
            model: "m".to_string(),
            previous_response_id: None,
            conversation_id: Some("conv_1".to_string()),
            metadata: None,
        };
        let object = envelope.object(Status::Incomplete, &[], None, Some("max_output_tokens"));

        assert_eq!(object["object"], "response");
        assert_eq!(object["created_at"], 1_794_300_000);
        assert_eq!(object["status"], "incomplete");
        assert_eq!(object["conversation"]["id"], "conv_1");
        assert_eq!(object["incomplete_details"]["reason"], "max_output_tokens");
        assert_eq!(object["metadata"], json!({}));
        assert_eq!(object["store"], true);
        assert!(object["previous_response_id"].is_null());
        assert!(object["usage"].is_null());
    }
}
