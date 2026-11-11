use std::error::Error as StdError;
use std::sync::Arc;
use std::time::Duration;

use std::pin::Pin;

use axum::http::header::{ACCEPT_ENCODING, CONTENT_ENCODING, HeaderName};
use axum::http::{HeaderMap, HeaderValue, Method};
use bytes::Bytes;
use futures_util::{Stream, StreamExt, stream};

use crate::codec::{self, Encoding};
use crate::config::Limits;
use crate::error::RelayError;
use crate::target::{NON_PUBLIC_MARKER, NonPublicAddress, PublicOnlyResolver, ResolvedTarget};

const HOP_BY_HOP: [&str; 6] = [
    "connection",
    "keep-alive",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];
const RELAY_SET_ON_REQUEST: [&str; 4] = ["host", "content-length", "content-encoding", "accept-encoding"];

fn is_hop_by_hop(name: &HeaderName) -> bool {
    let name = name.as_str();

    HOP_BY_HOP.contains(&name) || name.starts_with("proxy-")
}

/// Headers forwarded to the target: everything except the relay's own, hop-by-hop headers and
/// the ones the relay sets itself.
pub fn request_headers(incoming: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(incoming.len());

    for (name, value) in incoming {
        if name.as_str().starts_with("x-trout-")
            || is_hop_by_hop(name)
            || RELAY_SET_ON_REQUEST.contains(&name.as_str())
        {
            continue;
        }

        out.append(name.clone(), value.clone());
    }

    out
}

/// Headers returned to the client from the target's answer.
pub fn response_headers(upstream: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(upstream.len());

    for (name, value) in upstream {
        if is_hop_by_hop(name) || matches!(name.as_str(), "content-length" | "content-encoding") {
            continue;
        }

        out.append(name.clone(), value.clone());
    }

    out
}

pub type BodyStream = Pin<Box<dyn Stream<Item = Result<Bytes, RelayError>> + Send>>;

pub struct Upstream {
    public: reqwest::Client,
    private: reqwest::Client,
    first_byte_timeout: Duration,
    idle_timeout: Duration,
}

impl Upstream {
    pub fn new(limits: &Limits) -> Result<Self, reqwest::Error> {
        let base = || {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(limits.connect_timeout_secs))
                .no_proxy()
        };

        Ok(Self {
            public: base().dns_resolver(Arc::new(PublicOnlyResolver)).build()?,
            private: base().build()?,
            first_byte_timeout: Duration::from_secs(limits.first_byte_timeout_secs),
            idle_timeout: Duration::from_secs(limits.stream_idle_timeout_secs),
        })
    }

    pub async fn send(
        &self,
        target: &ResolvedTarget,
        method: Method,
        rest: &str,
        query: Option<&str>,
        incoming_headers: &HeaderMap,
        body: Bytes,
    ) -> Result<reqwest::Response, RelayError> {
        let client = if target.allow_private {
            &self.private
        } else {
            &self.public
        };
        let mut url = format!("{}/{}", target.base_url, rest);

        if let Some(query) = query {
            url.push('?');
            url.push_str(query);
        }

        let mut headers = request_headers(incoming_headers);

        headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("gzip"));

        let body = if target.accepts_gzip && !body.is_empty() {
            headers.insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));

            tokio::task::spawn_blocking(move || codec::gzip(&body))
                .await
                .map_err(|_| RelayError::Internal)?
                .map(Bytes::from)
                .map_err(|_| RelayError::Internal)?
        } else {
            body
        };
        let request = client.request(method, url).headers(headers).body(body);

        match tokio::time::timeout(self.first_byte_timeout, request.send()).await {
            Err(_) => Err(RelayError::UpstreamTimeout),
            Ok(Err(error)) => Err(map_send_error(&error)),
            Ok(Ok(response)) => Ok(response),
        }
    }

    /// The decoded answer body as a stream. Fails up front with `upstream_encoding_unsupported`
    /// when the target used an encoding the relay did not ask for.
    pub fn body_stream(&self, response: reqwest::Response) -> Result<BodyStream, RelayError> {
        match codec::content_encoding(response.headers()) {
            None => Err(RelayError::UpstreamEncodingUnsupported),
            Some(Encoding::Identity) => Ok(Box::pin(self.raw_stream(response))),
            Some(Encoding::Gzip) => Ok(Box::pin(codec::gunzip_stream(self.raw_stream(response)))),
        }
    }

    /// The answer body as sent, failing with `upstream_timeout` when the target goes quiet for
    /// longer than the idle limit. The stream ends after its first error.
    fn raw_stream(
        &self,
        response: reqwest::Response,
    ) -> impl Stream<Item = Result<Bytes, RelayError>> + Send + 'static {
        let idle = self.idle_timeout;
        let chunks = Box::pin(response.bytes_stream());

        stream::unfold(Some(chunks), move |state| async move {
            let mut chunks = state?;

            match tokio::time::timeout(idle, chunks.next()).await {
                Err(_) => Some((Err(RelayError::UpstreamTimeout), None)),
                Ok(None) => None,
                Ok(Some(Ok(chunk))) => Some((Ok(chunk), Some(chunks))),
                Ok(Some(Err(_))) => Some((
                    Err(RelayError::UpstreamUnreachable(
                        "connection lost mid-answer".to_string(),
                    )),
                    None,
                )),
            }
        })
    }
}

fn chain_has_non_public(error: &(dyn StdError + 'static)) -> bool {
    let mut current = Some(error);

    while let Some(e) = current {
        if e.downcast_ref::<NonPublicAddress>().is_some() || e.to_string().contains(NON_PUBLIC_MARKER) {
            return true;
        }

        current = e.source();
    }

    false
}

fn map_send_error(error: &reqwest::Error) -> RelayError {
    if chain_has_non_public(error) {
        return RelayError::TargetForbidden("address is not public".to_string());
    }

    if error.is_builder() {
        return RelayError::InvalidRequest("request path is not usable".to_string());
    }

    if error.is_connect() {
        return RelayError::UpstreamUnreachable("connection failed".to_string());
    }

    if error.is_timeout() {
        return RelayError::UpstreamTimeout;
    }

    RelayError::UpstreamUnreachable("request failed".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();

        for (name, value) in pairs {
            headers.append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }

        headers
    }

    #[test]
    fn request_filter_drops_relay_and_hop_headers() {
        let out = request_headers(&map(&[
            ("authorization", "Bearer provider"),
            ("content-type", "application/json"),
            ("x-custom", "kept"),
            ("x-trout-key", "secret"),
            ("x-trout-target", "https://a/v1"),
            ("x-trout-anything", "1"),
            ("connection", "keep-alive"),
            ("keep-alive", "timeout=5"),
            ("proxy-authorization", "Basic x"),
            ("te", "trailers"),
            ("trailer", "x"),
            ("transfer-encoding", "chunked"),
            ("upgrade", "h2c"),
            ("host", "relay.example.com"),
            ("content-length", "12"),
            ("content-encoding", "gzip"),
            ("accept-encoding", "gzip"),
        ]));
        let mut names: Vec<&str> = out.keys().map(|k| k.as_str()).collect();

        names.sort_unstable();

        assert_eq!(names, ["authorization", "content-type", "x-custom"]);
    }

    #[test]
    fn response_filter_keeps_provider_headers() {
        let out = response_headers(&map(&[
            ("content-type", "application/json"),
            ("x-request-id", "abc"),
            ("content-length", "12"),
            ("connection", "close"),
            ("transfer-encoding", "chunked"),
        ]));
        let mut names: Vec<&str> = out.keys().map(|k| k.as_str()).collect();

        names.sort_unstable();

        assert_eq!(names, ["content-type", "x-request-id"]);
    }
}
