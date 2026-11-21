use std::sync::Arc;

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;

use crate::AppState;
use crate::error::RelayError;
use crate::pages::{DASHBOARD_PATH, HOME_PATH, PROVIDERS_PATH};

pub const KEY_HEADER: &str = "x-trout-key";
pub const HEALTH_PATH: &str = "/healthz";

pub async fn require_key(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    // The pages are static and hold no relay data; the figures the dashboard shows come from
    // `/stats`, which needs the key like everything else.
    if matches!(
        request.uri().path(),
        HEALTH_PATH | HOME_PATH | DASHBOARD_PATH | PROVIDERS_PATH
    ) {
        return next.run(request).await;
    }

    let presented = request
        .headers()
        .get(KEY_HEADER)
        .map(|v| v.as_bytes())
        .unwrap_or_default();

    if !key_matches(presented, state.config.access_key.as_bytes()) {
        return RelayError::Unauthorized.into_response();
    }

    next.run(request).await
}

fn key_matches(presented: &[u8], expected: &[u8]) -> bool {
    !expected.is_empty() && bool::from(presented.ct_eq(expected))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_whole_key() {
        assert!(key_matches(b"secret-key", b"secret-key"));
        assert!(!key_matches(b"secret-ke", b"secret-key"));
        assert!(!key_matches(b"secret-key-", b"secret-key"));
        assert!(!key_matches(b"", b"secret-key"));
        assert!(!key_matches(b"", b""));
    }
}
