use serde_json::{Value, json};

use crate::error::RelayError;
use crate::responses::translate::{outcome, reasoning_text, usage};
use crate::responses::wire::Envelope;
use crate::store::{Completion, Status, new_id};

enum Item {
    Reasoning {
        id: String,
        text: String,
    },
    Message {
        id: String,
        text: String,
    },
    Call {
        id: String,
        call_id: String,
        name: String,
        arguments: String,
    },
}

impl Item {
    fn finished(&self) -> Value {
        match self {
            Self::Reasoning { id, text } => {
                json!({ "id": id, "type": "reasoning", "content": [{ "type": "reasoning_text", "text": text }] })
            }
            Self::Message { id, text } => json!({
                "id": id,
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": [{ "type": "output_text", "text": text }],
            }),
            Self::Call {
                id,
                call_id,
                name,
                arguments,
            } => json!({
                "id": id,
                "type": "function_call",
                "status": "completed",
                "call_id": call_id,
                "name": name,
                "arguments": arguments,
            }),
        }
    }
}

/// Turns the target's chat-completions event stream into the relay's `/v1/responses` event
/// stream, collecting the final output items on the way.
pub struct ChatStreamTranslator {
    envelope: Envelope,
    sequence: u64,
    buffer: Vec<u8>,
    items: Vec<Item>,
    reasoning_index: Option<usize>,
    message_index: Option<usize>,
    /// Output index for each upstream tool-call index.
    call_indexes: Vec<(u64, usize)>,
    finish_reason: Option<String>,
    usage: Option<Value>,
    done: bool,
}

impl ChatStreamTranslator {
    pub fn new(envelope: Envelope) -> Self {
        Self {
            envelope,
            sequence: 0,
            buffer: Vec::new(),
            items: Vec::new(),
            reasoning_index: None,
            message_index: None,
            call_indexes: Vec::new(),
            finish_reason: None,
            usage: None,
            done: false,
        }
    }

    fn event(&mut self, kind: &str, mut payload: Value) -> String {
        payload["type"] = json!(kind);
        payload["sequence_number"] = json!(self.sequence);
        self.sequence += 1;

        format!("event: {kind}\ndata: {payload}\n\n")
    }

    /// The two events that open every stream.
    pub fn start(&mut self) -> Vec<String> {
        let response = self.envelope.object(Status::InProgress, &[], None, None);

        vec![
            self.event("response.created", json!({ "response": response })),
            self.event("response.in_progress", json!({ "response": response })),
        ]
    }

    /// Feeds raw bytes from the target; returns the events they complete.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        let mut events = Vec::new();

        self.buffer.extend_from_slice(chunk);

        while let Some(end) = self.buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line);
            let Some(data) = line.trim_end().strip_prefix("data:") else {
                continue;
            };
            let data = data.trim_start();

            if data == "[DONE]" {
                self.done = true;

                continue;
            }

            if let Ok(chunk) = serde_json::from_str::<Value>(data) {
                self.apply(&chunk, &mut events);
            }
        }

        events
    }

    fn apply(&mut self, chunk: &Value, events: &mut Vec<String>) {
        if let Some(reported) = chunk.get("usage").and_then(usage) {
            self.usage = Some(reported);
        }

        let Some(choice) = chunk.get("choices").and_then(|choices| choices.get(0)) else {
            return;
        };
        let delta = &choice["delta"];

        if let Some(text) = reasoning_text(delta) {
            self.reasoning_delta(text, events);
        }

        if let Some(text) = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            self.text_delta(text, events);
        }

        for call in delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            self.call_delta(call, events);
        }

        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_string());
        }
    }

    fn reasoning_delta(&mut self, text: &str, events: &mut Vec<String>) {
        let index = match self.reasoning_index {
            Some(index) => index,
            None => {
                let id = new_id("rs");
                let index = self.items.len();

                events.push(self.event(
                    "response.output_item.added",
                    json!({ "output_index": index, "item": { "id": id, "type": "reasoning", "content": [] } }),
                ));
                self.items.push(Item::Reasoning {
                    id,
                    text: String::new(),
                });
                self.reasoning_index = Some(index);

                index
            }
        };
        let Item::Reasoning { id, text: collected } = &mut self.items[index] else {
            return;
        };
        let id = id.clone();

        collected.push_str(text);
        events.push(self.event(
            "response.reasoning_text.delta",
            json!({ "item_id": id, "output_index": index, "content_index": 0, "delta": text }),
        ));
    }

    fn text_delta(&mut self, text: &str, events: &mut Vec<String>) {
        let index = match self.message_index {
            Some(index) => index,
            None => {
                let id = new_id("msg");
                let index = self.items.len();

                events.push(self.event(
                    "response.output_item.added",
                    json!({
                        "output_index": index,
                        "item": { "id": id, "type": "message", "role": "assistant", "status": "in_progress", "content": [] },
                    }),
                ));
                events.push(self.event(
                    "response.content_part.added",
                    json!({
                        "item_id": id,
                        "output_index": index,
                        "content_index": 0,
                        "part": { "type": "output_text", "text": "" },
                    }),
                ));
                self.items.push(Item::Message {
                    id,
                    text: String::new(),
                });
                self.message_index = Some(index);

                index
            }
        };
        let Item::Message { id, text: collected } = &mut self.items[index] else {
            return;
        };
        let id = id.clone();

        collected.push_str(text);
        events.push(self.event(
            "response.output_text.delta",
            json!({ "item_id": id, "output_index": index, "content_index": 0, "delta": text }),
        ));
    }

    fn call_delta(&mut self, call: &Value, events: &mut Vec<String>) {
        let upstream_index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
        let function = &call["function"];
        let name = function.get("name").and_then(Value::as_str).unwrap_or_default();
        let arguments = function
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let known = self
            .call_indexes
            .iter()
            .find(|(upstream, _)| *upstream == upstream_index)
            .map(|(_, i)| *i);
        let index = match known {
            Some(index) => index,
            None => {
                let id = new_id("fc");
                let call_id = call
                    .get("id")
                    .and_then(Value::as_str)
                    .map_or_else(|| new_id("call"), str::to_string);
                let index = self.items.len();

                events.push(self.event(
                    "response.output_item.added",
                    json!({
                        "output_index": index,
                        "item": {
                            "id": id,
                            "type": "function_call",
                            "status": "in_progress",
                            "call_id": call_id,
                            "name": name,
                            "arguments": "",
                        },
                    }),
                ));
                self.items.push(Item::Call {
                    id,
                    call_id,
                    name: String::new(),
                    arguments: String::new(),
                });
                self.call_indexes.push((upstream_index, index));

                index
            }
        };
        let Item::Call {
            id,
            name: collected_name,
            arguments: collected,
            ..
        } = &mut self.items[index]
        else {
            return;
        };
        let id = id.clone();

        collected_name.push_str(name);
        collected.push_str(arguments);

        if !arguments.is_empty() {
            events.push(self.event(
                "response.function_call_arguments.delta",
                json!({ "item_id": id, "output_index": index, "delta": arguments }),
            ));
        }
    }

    /// Call when the target's stream has ended. Returns the events that close every item and the
    /// outcome to store; the caller emits [`Self::terminal`] once the outcome is recorded.
    pub fn finish(&mut self) -> Result<(Vec<String>, Completion), RelayError> {
        if self.finish_reason.is_none() {
            return Err(RelayError::UpstreamUnreachable(
                "answer ended before it was finished".to_string(),
            ));
        }

        let mut events = Vec::new();
        let finished: Vec<Value> = self.items.iter().map(Item::finished).collect();

        for (index, item) in finished.iter().enumerate() {
            let id = item["id"].clone();

            match item["type"].as_str() {
                Some("message") => {
                    let part = item["content"][0].clone();

                    events.push(self.event(
                        "response.output_text.done",
                        json!({ "item_id": id, "output_index": index, "content_index": 0, "text": part["text"] }),
                    ));
                    events.push(self.event(
                        "response.content_part.done",
                        json!({ "item_id": id, "output_index": index, "content_index": 0, "part": part }),
                    ));
                }
                Some("reasoning") => {
                    events.push(self.event(
                        "response.reasoning_text.done",
                        json!({
                            "item_id": id,
                            "output_index": index,
                            "content_index": 0,
                            "text": item["content"][0]["text"],
                        }),
                    ));
                }
                Some("function_call") => {
                    events.push(self.event(
                        "response.function_call_arguments.done",
                        json!({ "item_id": id, "output_index": index, "arguments": item["arguments"] }),
                    ));
                }
                _ => {}
            }

            events.push(self.event(
                "response.output_item.done",
                json!({ "output_index": index, "item": item }),
            ));
        }

        let (status, incomplete_reason) = outcome(self.finish_reason.as_deref());

        Ok((
            events,
            Completion {
                status: Some(status),
                output_items: finished,
                usage: self.usage.clone(),
                incomplete_reason,
            },
        ))
    }

    /// The last event of a successful stream, carrying the full response object.
    pub fn terminal(&mut self, completion: &Completion) -> String {
        let status = completion.status.unwrap_or(Status::Completed);
        let response = self.envelope.object(
            status,
            &completion.output_items,
            completion.usage.as_ref(),
            completion.incomplete_reason.as_deref(),
        );
        let kind = if status == Status::Incomplete {
            "response.incomplete"
        } else {
            "response.completed"
        };

        self.event(kind, json!({ "response": response }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translator() -> ChatStreamTranslator {
        ChatStreamTranslator::new(Envelope {
            id: "resp_1".to_string(),
            created_at_ms: 0,
            model: "m".to_string(),
            previous_response_id: None,
            conversation_id: Some("conv_1".to_string()),
            metadata: None,
        })
    }

    fn chunk(delta: Value, finish: Option<&str>) -> String {
        format!(
            "data: {}\n\n",
            json!({ "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }] })
        )
    }

    fn parse(events: &[String]) -> Vec<(String, Value)> {
        events
            .iter()
            .map(|event| {
                let (head, data) = event.trim_end().split_once("\ndata: ").unwrap();
                let value: Value = serde_json::from_str(data).unwrap();

                assert_eq!(
                    head.strip_prefix("event: ").unwrap(),
                    value["type"].as_str().unwrap()
                );

                (value["type"].as_str().unwrap().to_string(), value)
            })
            .collect()
    }

    fn kinds(events: &[(String, Value)]) -> Vec<&str> {
        events.iter().map(|(kind, _)| kind.as_str()).collect()
    }

    #[test]
    fn text_answer_split_across_arbitrary_byte_boundaries() {
        let mut t = translator();
        let mut all = t.start();
        let upstream = [
            chunk(json!({ "role": "assistant", "content": "" }), None),
            chunk(json!({ "content": "Hel" }), None),
            chunk(json!({ "content": "lo" }), None),
            chunk(json!({}), Some("stop")),
            format!("data: {}\n\n", json!({ "choices": [], "usage": { "prompt_tokens": 7, "completion_tokens": 2, "total_tokens": 9 } })),
            "data: [DONE]\n\n".to_string(),
        ]
        .concat();

        for piece in upstream.as_bytes().chunks(7) {
            all.extend(t.feed(piece));
        }

        let (closing, completion) = t.finish().unwrap();

        all.extend(closing);
        all.push(t.terminal(&completion));

        let events = parse(&all);

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

        for (expected, (_, value)) in events.iter().enumerate() {
            assert_eq!(value["sequence_number"], expected);
        }

        assert_eq!(events[0].1["response"]["status"], "in_progress");
        assert_eq!(events[0].1["response"]["id"], "resp_1");
        assert_eq!(events[4].1["delta"], "Hel");
        assert_eq!(events[6].1["text"], "Hello");

        let last = &events.last().unwrap().1["response"];

        assert_eq!(last["status"], "completed");
        assert_eq!(last["output"][0]["content"][0]["text"], "Hello");
        assert_eq!(
            last["usage"],
            json!({ "input_tokens": 7, "output_tokens": 2, "total_tokens": 9 })
        );
        assert_eq!(completion.status, Some(Status::Completed));
    }

    #[test]
    fn reasoning_then_text_then_parallel_tool_calls() {
        let mut t = translator();
        let mut all = Vec::new();

        for piece in [
            chunk(json!({ "reasoning_content": "think " }), None),
            chunk(json!({ "reasoning_content": "more" }), None),
            chunk(json!({ "content": "Reading." }), None),
            chunk(
                json!({ "tool_calls": [{ "index": 0, "id": "call_a", "type": "function", "function": { "name": "FileRead", "arguments": "" } }] }),
                None,
            ),
            chunk(
                json!({ "tool_calls": [{ "index": 0, "function": { "arguments": "{\"path\":" } }] }),
                None,
            ),
            chunk(
                json!({ "tool_calls": [{ "index": 1, "id": "call_b", "type": "function", "function": { "name": "Grep", "arguments": "{}" } }] }),
                None,
            ),
            chunk(
                json!({ "tool_calls": [{ "index": 0, "function": { "arguments": "\"a\"}" } }] }),
                None,
            ),
            chunk(json!({}), Some("tool_calls")),
        ] {
            all.extend(t.feed(piece.as_bytes()));
        }

        let (closing, completion) = t.finish().unwrap();
        let live = parse(&all);
        let closing = parse(&closing);
        let outputs: Vec<(&str, &str)> = completion
            .output_items
            .iter()
            .map(|item| (item["type"].as_str().unwrap(), item["id"].as_str().unwrap()))
            .collect();

        assert_eq!(
            outputs.iter().map(|(kind, _)| *kind).collect::<Vec<_>>(),
            ["reasoning", "message", "function_call", "function_call"]
        );
        assert_eq!(completion.output_items[0]["content"][0]["text"], "think more");
        assert_eq!(completion.output_items[2]["call_id"], "call_a");
        assert_eq!(completion.output_items[2]["name"], "FileRead");
        assert_eq!(completion.output_items[2]["arguments"], "{\"path\":\"a\"}");
        assert_eq!(completion.output_items[3]["call_id"], "call_b");
        assert_eq!(completion.output_items[3]["arguments"], "{}");

        let added: Vec<u64> = live
            .iter()
            .filter(|(kind, _)| kind == "response.output_item.added")
            .map(|(_, value)| value["output_index"].as_u64().unwrap())
            .collect();

        assert_eq!(added, [0, 1, 2, 3]);
        assert_eq!(
            live.iter()
                .filter(|(kind, _)| kind == "response.reasoning_text.delta")
                .count(),
            2
        );
        assert_eq!(
            live.iter()
                .filter(|(kind, _)| kind == "response.function_call_arguments.delta")
                .count(),
            3
        );
        assert_eq!(
            kinds(&closing),
            [
                "response.reasoning_text.done",
                "response.output_item.done",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.function_call_arguments.done",
                "response.output_item.done",
                "response.function_call_arguments.done",
                "response.output_item.done",
            ]
        );
        assert_eq!(closing[5].1["arguments"], "{\"path\":\"a\"}");
        assert_eq!(closing[5].1["item_id"], outputs[2].1);
    }

    #[test]
    fn length_finish_ends_with_incomplete() {
        let mut t = translator();

        t.feed(chunk(json!({ "content": "cut" }), Some("length")).as_bytes());

        let (_, completion) = t.finish().unwrap();
        let terminal = parse(&[t.terminal(&completion)]);

        assert_eq!(terminal[0].0, "response.incomplete");
        assert_eq!(
            terminal[0].1["response"]["incomplete_details"]["reason"],
            "max_output_tokens"
        );
    }

    #[test]
    fn stream_that_ends_without_a_finish_reason_is_a_failure() {
        let mut t = translator();

        t.feed(chunk(json!({ "content": "half" }), None).as_bytes());

        assert!(matches!(t.finish(), Err(RelayError::UpstreamUnreachable(_))));
    }

    #[test]
    fn comments_and_unparsable_lines_are_ignored() {
        let mut t = translator();
        let events = t.feed(b": keep-alive\n\nevent: ping\ndata: not json\n\n");

        assert!(events.is_empty());
    }
}
