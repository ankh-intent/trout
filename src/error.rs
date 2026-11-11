use axum::Json;
use axum::http::header::ACCEPT_ENCODING;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;

pub const SOURCE_HEADER: &str = "x-trout-source";
pub const SOURCE_RELAY: &str = "relay";
pub const SOURCE_UPSTREAM: &str = "upstream";

/// Errors produced by the relay itself. Codes are a stable contract; provider errors never pass
/// through this type.
#[derive(Debug, Clone, thiserror::Error)]
pub enum RelayError {
    #[error("X-Trout-Key header is missing or wrong")]
    Unauthorized,
    #[error("X-Trout-Target header is required")]
    TargetMissing,
    #[error("X-Trout-Target is not usable: {0}")]
    TargetInvalid(String),
    #[error("target is not permitted: {0}")]
    TargetForbidden(String),
    #[error("no such route")]
    RouteNotFound,
    #[error("request body exceeds the configured limit")]
    BodyTooLarge,
    #[error("unsupported Content-Encoding; supported: gzip")]
    EncodingUnsupported,
    #[error("request body does not decode as gzip")]
    BodyCorrupt,
    #[error("target is unreachable: {0}")]
    UpstreamUnreachable(String),
    #[error("target answered with an unsupported Content-Encoding")]
    UpstreamEncodingUnsupported,
    #[error("target did not answer in time")]
    UpstreamTimeout,
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("unsupported feature: {0}")]
    UnsupportedFeature(String),
    #[error("response not found")]
    ResponseNotFound,
    #[error("response is still in progress")]
    ResponseInProgress,
    #[error("response is incomplete and cannot be continued")]
    ResponseNotContinuable,
    #[error("conversation belongs to a different target")]
    TargetMismatch,
    #[error("internal relay error")]
    Internal,
}

impl RelayError {
    pub const ALL_CODES: [&'static str; 18] = [
        "unauthorized",
        "target_missing",
        "target_invalid",
        "target_forbidden",
        "route_not_found",
        "body_too_large",
        "encoding_unsupported",
        "body_corrupt",
        "upstream_unreachable",
        "upstream_encoding_unsupported",
        "upstream_timeout",
        "invalid_request",
        "unsupported_feature",
        "response_not_found",
        "response_in_progress",
        "response_not_continuable",
        "target_mismatch",
        "internal",
    ];

    pub fn code(&self) -> &'static str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::TargetMissing => "target_missing",
            Self::TargetInvalid(_) => "target_invalid",
            Self::TargetForbidden(_) => "target_forbidden",
            Self::RouteNotFound => "route_not_found",
            Self::BodyTooLarge => "body_too_large",
            Self::EncodingUnsupported => "encoding_unsupported",
            Self::BodyCorrupt => "body_corrupt",
            Self::UpstreamUnreachable(_) => "upstream_unreachable",
            Self::UpstreamEncodingUnsupported => "upstream_encoding_unsupported",
            Self::UpstreamTimeout => "upstream_timeout",
            Self::InvalidRequest(_) => "invalid_request",
            Self::UnsupportedFeature(_) => "unsupported_feature",
            Self::ResponseNotFound => "response_not_found",
            Self::ResponseInProgress => "response_in_progress",
            Self::ResponseNotContinuable => "response_not_continuable",
            Self::TargetMismatch => "target_mismatch",
            Self::Internal => "internal",
        }
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::TargetMissing
            | Self::TargetInvalid(_)
            | Self::BodyCorrupt
            | Self::InvalidRequest(_)
            | Self::UnsupportedFeature(_) => StatusCode::BAD_REQUEST,
            Self::TargetForbidden(_) => StatusCode::FORBIDDEN,
            Self::RouteNotFound | Self::ResponseNotFound => StatusCode::NOT_FOUND,
            Self::BodyTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::EncodingUnsupported => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Self::ResponseInProgress | Self::ResponseNotContinuable | Self::TargetMismatch => {
                StatusCode::CONFLICT
            }
            Self::UpstreamUnreachable(_) | Self::UpstreamEncodingUnsupported => StatusCode::BAD_GATEWAY,
            Self::UpstreamTimeout => StatusCode::GATEWAY_TIMEOUT,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn body(&self) -> serde_json::Value {
        json!({ "error": { "type": "trout_error", "code": self.code(), "message": self.to_string() } })
    }

    /// The final event written to an already started `text/event-stream` answer.
    pub fn sse_event(&self) -> String {
        format!("event: error\ndata: {}\n\n", self.body())
    }
}

impl IntoResponse for RelayError {
    fn into_response(self) -> Response {
        let mut response = (self.status(), Json(self.body())).into_response();
        let headers = response.headers_mut();

        headers.insert(SOURCE_HEADER, HeaderValue::from_static(SOURCE_RELAY));

        if matches!(self, Self::EncodingUnsupported) {
            headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("gzip"));
        }

        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> Vec<(RelayError, u16)> {
        vec![
            (RelayError::Unauthorized, 401),
            (RelayError::TargetMissing, 400),
            (RelayError::TargetInvalid("x".into()), 400),
            (RelayError::TargetForbidden("x".into()), 403),
            (RelayError::RouteNotFound, 404),
            (RelayError::BodyTooLarge, 413),
            (RelayError::EncodingUnsupported, 415),
            (RelayError::BodyCorrupt, 400),
            (RelayError::UpstreamUnreachable("x".into()), 502),
            (RelayError::UpstreamEncodingUnsupported, 502),
            (RelayError::UpstreamTimeout, 504),
            (RelayError::InvalidRequest("x".into()), 400),
            (RelayError::UnsupportedFeature("x".into()), 400),
            (RelayError::ResponseNotFound, 404),
            (RelayError::ResponseInProgress, 409),
            (RelayError::ResponseNotContinuable, 409),
            (RelayError::TargetMismatch, 409),
            (RelayError::Internal, 500),
        ]
    }

    #[test]
    fn every_code_has_its_status_and_shape() {
        let cases = all();
        let codes: Vec<&str> = cases.iter().map(|(e, _)| e.code()).collect();

        assert_eq!(codes, RelayError::ALL_CODES);

        for (error, status) in cases {
            let body = error.body();
            let response = error.clone().into_response();

            assert_eq!(response.status().as_u16(), status, "{}", error.code());
            assert_eq!(response.headers().get(SOURCE_HEADER).unwrap(), SOURCE_RELAY);
            assert_eq!(body["error"]["type"], "trout_error");
            assert_eq!(body["error"]["code"], error.code());
            assert!(body["error"]["message"].as_str().is_some_and(|m| !m.is_empty()));
        }
    }

    #[test]
    fn encoding_unsupported_names_supported_encodings() {
        let response = RelayError::EncodingUnsupported.into_response();

        assert_eq!(response.headers().get(ACCEPT_ENCODING).unwrap(), "gzip");
    }

    #[test]
    fn sse_event_is_one_complete_event() {
        let event = RelayError::UpstreamTimeout.sse_event();

        assert!(event.starts_with("event: error\ndata: {"));
        assert!(event.ends_with("\n\n"));
        assert!(event.contains("\"code\":\"upstream_timeout\""));
    }
}
