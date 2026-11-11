use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_ENCODING, CONTENT_TYPE, VARY};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::{StreamExt, stream};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::AppState;
use crate::codec;
use crate::error::{RelayError, SOURCE_HEADER, SOURCE_RELAY};
use crate::proxy::{read_decoded_body, relay_answer};
use crate::responses::stream::ChatStreamTranslator;
use crate::responses::translate::{from_chat_completion, to_chat_request};
use crate::responses::wire::{self, Envelope};
use crate::store::{Completion, NewTurn, Status, Store};
use crate::target::{self, ResolvedTarget};
use crate::telemetry::{Counters, TargetHost};
use crate::upstream::BodyStream;

const CHAT_PATH: &str = "chat/completions";

/// Forgets a turn that did not finish, including when the request future is dropped because the
/// client went away.
struct TurnGuard {
    store: Store,
    response_id: String,
    armed: bool,
}

impl TurnGuard {
    fn new(store: Store, response_id: String) -> Self {
        Self {
            store,
            response_id,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }

    async fn abort(mut self) {
        self.armed = false;

        let _ = self.store.abort(self.response_id.clone()).await;
    }
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        if self.armed {
            let store = self.store.clone();
            let response_id = self.response_id.clone();

            tokio::spawn(async move {
                let _ = store.abort(response_id).await;
            });
        }
    }
}

fn relay_json(
    status: StatusCode,
    value: &Value,
    request_headers: &HeaderMap,
    counters: Option<&Counters>,
) -> Result<Response, RelayError> {
    let plain = serde_json::to_vec(value).map_err(|_| RelayError::Internal)?;
    let gzip = codec::accepts_gzip(request_headers);

    if let (true, Some(counters)) = (gzip, counters) {
        counters.plain_out.store(plain.len() as u64, Ordering::Relaxed);
    }

    let body = if gzip {
        codec::gzip(&plain).map_err(|_| RelayError::Internal)?
    } else {
        plain
    };
    let mut response = Response::new(Body::from(body));
    let headers = response.headers_mut();

    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(SOURCE_HEADER, HeaderValue::from_static(SOURCE_RELAY));
    headers.insert(VARY, HeaderValue::from_static("accept-encoding"));

    if gzip {
        headers.insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    }

    *response.status_mut() = status;

    Ok(response)
}

/// `POST /v1/responses`
pub async fn create(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let target = match target::resolve(request.headers(), &state.config) {
        Ok(target) => target,
        Err(error) => return error.into_response(),
    };
    let mut response = match create_turn(&state, &target, request).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    };

    response.extensions_mut().insert(TargetHost(target.host));

    response
}

async fn create_turn(
    state: &Arc<AppState>,
    target: &ResolvedTarget,
    request: Request,
) -> Result<Response, RelayError> {
    let (parts, body) = request.into_parts();
    let limits = &state.config.limits;
    let body = read_decoded_body(
        &parts.headers,
        body,
        limits.max_body_bytes,
        limits.max_decoded_body_bytes,
    )
    .await?;

    let counters = parts.extensions.get::<Counters>().cloned();

    if let Some(counters) = &counters {
        counters.decoded_in.store(body.len() as u64, Ordering::Relaxed);
    }

    let request = wire::parse_request(&body)?;
    let begun = state
        .store
        .begin(NewTurn {
            target: target.base_url.clone(),
            previous_response_id: request.previous_response_id.clone(),
            overrides: request.overrides,
            input_items: request.input.clone(),
            metadata: request.metadata.clone(),
            persist: request.store,
        })
        .await?;
    let guard = TurnGuard::new(state.store.clone(), begun.response_id.clone());
    let mut items = begun.history;

    items.extend(request.input);

    let chat = to_chat_request(&begun.settings, &items, request.stream);
    let envelope = Envelope {
        id: begun.response_id,
        created_at_ms: begun.created_at_ms,
        model: begun
            .settings
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        previous_response_id: begun.previous_response_id,
        conversation_id: begun.conversation_id,
        metadata: request.metadata,
    };
    let mut upstream_headers = parts.headers.clone();

    upstream_headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

    let chat = Bytes::from(chat.to_string());

    if let Some(counters) = &counters {
        counters.mark_forwarded(chat.len());
    }

    let sent = state
        .upstream
        .send(target, Method::POST, CHAT_PATH, None, &upstream_headers, chat)
        .await;
    let upstream = match sent {
        Ok(upstream) => upstream,
        Err(error) => {
            guard.abort().await;

            return Err(error);
        }
    };

    if !upstream.status().is_success() {
        guard.abort().await;

        return relay_answer(state, upstream, &parts.headers, &Method::POST, counters.as_ref());
    }

    let body = match state.upstream.body_stream(upstream) {
        Ok(body) => body,
        Err(error) => {
            guard.abort().await;

            return Err(error);
        }
    };

    if request.stream {
        return Ok(streamed(
            state.store.clone(),
            guard,
            envelope,
            body,
            &parts.headers,
            counters,
        ));
    }

    match collect_completion(body).await {
        Ok(completion) => {
            let mut guard = guard;

            if let Err(error) = state
                .store
                .finalise(envelope.id.clone(), completion.clone())
                .await
            {
                guard.abort().await;

                return Err(error);
            }

            guard.disarm();

            let object = envelope.object(
                completion.status.unwrap_or(Status::Completed),
                &completion.output_items,
                completion.usage.as_ref(),
                completion.incomplete_reason.as_deref(),
            );

            relay_json(StatusCode::OK, &object, &parts.headers, counters.as_ref())
        }
        Err(error) => {
            guard.abort().await;

            Err(error)
        }
    }
}

async fn collect_completion(mut body: BodyStream) -> Result<Completion, RelayError> {
    let mut bytes = Vec::new();

    while let Some(chunk) = body.next().await {
        bytes.extend_from_slice(&chunk?);
    }

    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| RelayError::UpstreamUnreachable("answer is not valid JSON".to_string()))?;

    from_chat_completion(&value)
}

fn streamed(
    store: Store,
    guard: TurnGuard,
    envelope: Envelope,
    body: BodyStream,
    request_headers: &HeaderMap,
    counters: Option<Counters>,
) -> Response {
    let (tx, rx) = mpsc::channel::<Result<Bytes, RelayError>>(64);

    tokio::spawn(drive_stream(store, guard, envelope, body, tx));

    let events = stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) });
    let gzip = codec::accepts_gzip(request_headers);
    let body = match (gzip, counters) {
        (true, Some(counters)) => Body::from_stream(codec::gzip_stream(codec::counted(
            events,
            counters.plain_out.clone(),
        ))),
        (true, None) => Body::from_stream(codec::gzip_stream(events)),
        (false, _) => Body::from_stream(events),
    };
    let mut response = Response::new(body);
    let headers = response.headers_mut();

    headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert(SOURCE_HEADER, HeaderValue::from_static(SOURCE_RELAY));
    headers.insert(VARY, HeaderValue::from_static("accept-encoding"));

    if gzip {
        headers.insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    }

    response
}

type EventSender = mpsc::Sender<Result<Bytes, RelayError>>;

async fn send_all(tx: &EventSender, events: Vec<String>) -> bool {
    for event in events {
        if tx.send(Ok(Bytes::from(event))).await.is_err() {
            return false;
        }
    }

    true
}

async fn fail(tx: &EventSender, guard: TurnGuard, error: &RelayError) {
    // Forget the turn first, so nothing is stored by the time the client reads the error.
    guard.abort().await;

    let _ = tx.send(Ok(Bytes::from(error.sse_event()))).await;
}

/// Pumps the target's stream into response events. Ends, forgetting the turn, as soon as the
/// client is gone or the target fails.
async fn drive_stream(
    store: Store,
    mut guard: TurnGuard,
    envelope: Envelope,
    mut body: BodyStream,
    tx: EventSender,
) {
    let response_id = envelope.id.clone();
    let mut translator = ChatStreamTranslator::new(envelope);

    if !send_all(&tx, translator.start()).await {
        return guard.abort().await;
    }

    loop {
        tokio::select! {
            () = tx.closed() => return guard.abort().await,
            item = body.next() => match item {
                Some(Ok(chunk)) => {
                    if !send_all(&tx, translator.feed(&chunk)).await {
                        return guard.abort().await;
                    }
                }
                Some(Err(error)) => return fail(&tx, guard, &error).await,
                None => break,
            },
        }
    }

    let (closing, completion) = match translator.finish() {
        Ok(finished) => finished,
        Err(error) => return fail(&tx, guard, &error).await,
    };

    if !send_all(&tx, closing).await {
        return guard.abort().await;
    }

    // Recorded before the final event goes out: once the client sees it, the id is continuable.
    if let Err(error) = store.finalise(response_id, completion.clone()).await {
        return fail(&tx, guard, &error).await;
    }

    guard.disarm();

    let _ = tx.send(Ok(Bytes::from(translator.terminal(&completion)))).await;
}

/// `GET /v1/responses/{id}`
pub async fn retrieve(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, RelayError> {
    let stored = state.store.get(id).await?;
    let envelope = Envelope {
        id: stored.id,
        created_at_ms: stored.created_at_ms,
        model: stored
            .settings
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        previous_response_id: stored.parent_id,
        conversation_id: Some(stored.conversation_id),
        metadata: stored.metadata,
    };
    let object = envelope.object(
        stored.status,
        &stored.output_items,
        stored.usage.as_ref(),
        stored.incomplete_reason.as_deref(),
    );

    relay_json(StatusCode::OK, &object, &headers, None)
}

/// `DELETE /v1/responses/{id}`
pub async fn delete_response(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, RelayError> {
    state.store.delete_response(id.clone()).await?;

    relay_json(StatusCode::OK, &wire::deleted_response(&id), &headers, None)
}

/// `DELETE /v1/conversations/{id}`
pub async fn delete_conversation(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, RelayError> {
    state.store.delete_conversation(id.clone()).await?;

    relay_json(StatusCode::OK, &wire::deleted_conversation(&id), &headers, None)
}
