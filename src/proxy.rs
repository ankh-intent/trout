use std::error::Error as StdError;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::{CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, VARY};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::LengthLimitError;

use crate::AppState;
use crate::codec;
use crate::error::{RelayError, SOURCE_HEADER, SOURCE_UPSTREAM};
use crate::target::{self, ResolvedTarget};
use crate::telemetry::{Counters, TargetHost};
use crate::upstream::response_headers;

pub const ROUTE_PREFIX: &str = "/v1/";

/// Catch-all for `/v1/*`: forwards the request to the target and streams the answer back.
pub async fn forward(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let target = match target::resolve(request.headers(), &state.config) {
        Ok(target) => target,
        Err(error) => return error.into_response(),
    };
    let mut response = match forward_to(&state, &target, request).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    };

    response.extensions_mut().insert(TargetHost(target.host));

    response
}

async fn forward_to(
    state: &AppState,
    target: &ResolvedTarget,
    request: Request,
) -> Result<Response, RelayError> {
    let (parts, body) = request.into_parts();
    let rest = parts
        .uri
        .path()
        .strip_prefix(ROUTE_PREFIX)
        .ok_or(RelayError::RouteNotFound)?;
    let limits = &state.config.limits;
    let body = read_decoded_body(
        &parts.headers,
        body,
        limits.max_body_bytes,
        limits.max_decoded_body_bytes,
    )
    .await?;

    if let Some(counters) = parts.extensions.get::<Counters>() {
        counters.decoded_in.store(body.len() as u64, Ordering::Relaxed);
    }

    let upstream = state
        .upstream
        .send(
            target,
            parts.method.clone(),
            rest,
            parts.uri.query(),
            &parts.headers,
            body,
        )
        .await?;

    relay_answer(state, upstream, &parts.headers, &parts.method)
}

/// Returns the target's answer to the client: status and headers unchanged, body streamed and
/// gzip-encoded when the client accepts it.
pub fn relay_answer(
    state: &AppState,
    upstream: reqwest::Response,
    request_headers: &HeaderMap,
    method: &Method,
) -> Result<Response, RelayError> {
    let status = upstream.status();
    let mut headers = response_headers(upstream.headers());

    headers.insert(SOURCE_HEADER, HeaderValue::from_static(SOURCE_UPSTREAM));
    headers.append(VARY, HeaderValue::from_static("accept-encoding"));

    let is_event_stream = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.trim_start()
                .to_ascii_lowercase()
                .starts_with("text/event-stream")
        });
    let chunks = state.upstream.body_stream(upstream)?.map(move |item| match item {
        Ok(chunk) => Ok(chunk),
        // The status is already sent; an event stream gets one final error event, any other
        // answer is cut so the client sees a broken transfer instead of a short "success".
        Err(error) if is_event_stream => Ok(Bytes::from(error.sse_event())),
        Err(error) => Err(error),
    });
    let has_body = method != Method::HEAD
        && !status.is_informational()
        && status != StatusCode::NO_CONTENT
        && status != StatusCode::NOT_MODIFIED;
    let body = if has_body && codec::accepts_gzip(request_headers) {
        headers.insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));

        Body::from_stream(codec::gzip_stream(chunks))
    } else {
        Body::from_stream(chunks)
    };
    let mut response = Response::new(body);

    *response.status_mut() = status;
    *response.headers_mut() = headers;

    Ok(response)
}

/// Reads the whole request body, enforcing the wire limit, then decodes it per its
/// `Content-Encoding`, enforcing the decoded limit.
pub async fn read_decoded_body(
    headers: &HeaderMap,
    body: Body,
    max_wire: usize,
    max_decoded: usize,
) -> Result<Bytes, RelayError> {
    if codec::content_encoding(headers).is_none() {
        return Err(RelayError::EncodingUnsupported);
    }

    let wire = read_body(headers, body, max_wire).await?;

    codec::decode_request_body(headers, wire, max_decoded).await
}

/// Reads the whole request body, refusing anything over the wire limit.
pub async fn read_body(headers: &HeaderMap, body: Body, max_bytes: usize) -> Result<Bytes, RelayError> {
    let declared = headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());

    if declared.is_some_and(|len| len > max_bytes as u64) {
        return Err(RelayError::BodyTooLarge);
    }

    axum::body::to_bytes(body, max_bytes).await.map_err(|error| {
        if chain_has_length_limit(&error) {
            RelayError::BodyTooLarge
        } else {
            RelayError::InvalidRequest("request body could not be read".to_string())
        }
    })
}

fn chain_has_length_limit(error: &(dyn StdError + 'static)) -> bool {
    let mut current = Some(error);

    while let Some(e) = current {
        if e.downcast_ref::<LengthLimitError>().is_some() {
            return true;
        }

        current = e.source();
    }

    false
}
