use std::io::{self, Read, Write};

use axum::http::HeaderMap;
use axum::http::header::{ACCEPT_ENCODING, CONTENT_ENCODING};
use bytes::Bytes;
use flate2::Compression;
use flate2::read::MultiGzDecoder;
use flate2::write::{GzDecoder, GzEncoder};
use futures_util::{Stream, StreamExt, stream};

use crate::error::RelayError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Identity,
    Gzip,
}

/// Reads a `Content-Encoding` header. `None` means an encoding the relay does not handle.
pub fn content_encoding(headers: &HeaderMap) -> Option<Encoding> {
    let Some(value) = headers.get(CONTENT_ENCODING) else {
        return Some(Encoding::Identity);
    };

    match value.to_str().ok()?.trim().to_ascii_lowercase().as_str() {
        "" | "identity" => Some(Encoding::Identity),
        "gzip" | "x-gzip" => Some(Encoding::Gzip),
        _ => None,
    }
}

/// Whether the client's `Accept-Encoding` permits a gzip answer.
pub fn accepts_gzip(headers: &HeaderMap) -> bool {
    let mut wildcard = None;

    for value in headers.get_all(ACCEPT_ENCODING) {
        let Ok(value) = value.to_str() else { continue };

        for item in value.split(',') {
            let mut parts = item.split(';');
            let coding = parts.next().unwrap_or_default().trim().to_ascii_lowercase();
            let allowed = parts
                .filter_map(|p| p.trim().strip_prefix("q="))
                .next()
                .and_then(|q| q.trim().parse::<f32>().ok())
                .is_none_or(|q| q > 0.0);

            match coding.as_str() {
                "gzip" | "x-gzip" => return allowed,
                "*" => wildcard = Some(allowed),
                _ => {}
            }
        }
    }

    wildcard.unwrap_or(false)
}

pub fn gzip(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::with_capacity(data.len() / 4 + 64), Compression::default());

    encoder.write_all(data)?;
    encoder.finish()
}

/// Inflates a gzip body, stopping as soon as the decoded size passes `max_decoded`.
fn gunzip_capped(data: &[u8], max_decoded: usize) -> Result<Vec<u8>, RelayError> {
    let mut decoder = MultiGzDecoder::new(data);
    let mut out = Vec::with_capacity(data.len().saturating_mul(4).min(max_decoded));
    let mut buffer = vec![0u8; 64 * 1024];

    loop {
        let read = decoder.read(&mut buffer).map_err(|_| RelayError::BodyCorrupt)?;

        if read == 0 {
            return Ok(out);
        }

        if out.len() + read > max_decoded {
            return Err(RelayError::BodyTooLarge);
        }

        out.extend_from_slice(&buffer[..read]);
    }
}

/// Turns the body as received into its decoded form, per the request's `Content-Encoding`.
pub async fn decode_request_body(
    headers: &HeaderMap,
    body: Bytes,
    max_decoded: usize,
) -> Result<Bytes, RelayError> {
    match content_encoding(headers) {
        None => Err(RelayError::EncodingUnsupported),
        Some(Encoding::Identity) => Ok(body),
        Some(Encoding::Gzip) => {
            tokio::task::spawn_blocking(move || gunzip_capped(&body, max_decoded).map(Bytes::from))
                .await
                .map_err(|_| RelayError::Internal)?
        }
    }
}

/// Gzip encoder that makes every pushed chunk decodable on arrival: each `push` ends with a sync
/// flush, so a streamed event is never held back waiting for later data.
pub struct GzipChunkEncoder {
    inner: GzEncoder<Vec<u8>>,
}

impl Default for GzipChunkEncoder {
    fn default() -> Self {
        Self {
            inner: GzEncoder::new(Vec::new(), Compression::default()),
        }
    }
}

impl GzipChunkEncoder {
    pub fn push(&mut self, chunk: &[u8]) -> io::Result<Bytes> {
        self.inner.write_all(chunk)?;
        self.inner.flush()?;

        Ok(Bytes::from(std::mem::take(self.inner.get_mut())))
    }

    pub fn finish(self) -> io::Result<Bytes> {
        self.inner.finish().map(Bytes::from)
    }
}

/// Adds the size of every chunk that passes through to `counter`.
pub fn counted<S>(
    inner: S,
    counter: std::sync::Arc<std::sync::atomic::AtomicU64>,
) -> impl Stream<Item = Result<Bytes, RelayError>> + Send + 'static
where
    S: Stream<Item = Result<Bytes, RelayError>> + Send + 'static,
{
    inner.inspect(move |item| {
        if let Ok(chunk) = item {
            counter.fetch_add(chunk.len() as u64, std::sync::atomic::Ordering::Relaxed);
        }
    })
}

/// Gzip-encodes a byte stream chunk by chunk. An error from the inner stream ends the output.
pub fn gzip_stream<S>(inner: S) -> impl Stream<Item = Result<Bytes, RelayError>> + Send + 'static
where
    S: Stream<Item = Result<Bytes, RelayError>> + Send + 'static,
{
    let state = Some((Box::pin(inner), GzipChunkEncoder::default()));

    stream::unfold(state, |state| async move {
        let (mut inner, mut encoder) = state?;

        loop {
            match inner.next().await {
                Some(Ok(chunk)) => match encoder.push(&chunk) {
                    Ok(out) if out.is_empty() => {}
                    Ok(out) => return Some((Ok(out), Some((inner, encoder)))),
                    Err(_) => return Some((Err(RelayError::Internal), None)),
                },
                Some(Err(error)) => return Some((Err(error), None)),
                None => {
                    return match encoder.finish() {
                        Ok(out) => Some((Ok(out), None)),
                        Err(_) => Some((Err(RelayError::Internal), None)),
                    };
                }
            }
        }
    })
}

/// Decodes a gzip byte stream from the target chunk by chunk.
pub fn gunzip_stream<S>(inner: S) -> impl Stream<Item = Result<Bytes, RelayError>> + Send + 'static
where
    S: Stream<Item = Result<Bytes, RelayError>> + Send + 'static,
{
    let undecodable = || RelayError::UpstreamUnreachable("answer did not decode as gzip".to_string());
    let state = Some((Box::pin(inner), GzDecoder::new(Vec::new())));

    stream::unfold(state, move |state| async move {
        let (mut inner, mut decoder) = state?;

        loop {
            match inner.next().await {
                Some(Ok(chunk)) => {
                    if decoder.write_all(&chunk).and_then(|()| decoder.flush()).is_err() {
                        return Some((Err(undecodable()), None));
                    }

                    let out = std::mem::take(decoder.get_mut());

                    if !out.is_empty() {
                        return Some((Ok(Bytes::from(out)), Some((inner, decoder))));
                    }
                }
                Some(Err(error)) => return Some((Err(error), None)),
                None => {
                    return match decoder.finish() {
                        Ok(out) if out.is_empty() => None,
                        Ok(out) => Some((Ok(Bytes::from(out)), None)),
                        Err(_) => Some((Err(undecodable()), None)),
                    };
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(name: &'static str, value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();

        headers.insert(name, value.parse().unwrap());

        headers
    }

    fn gunzip(data: &[u8]) -> Vec<u8> {
        gunzip_capped(data, usize::MAX).unwrap()
    }

    #[test]
    fn accept_encoding_negotiation() {
        for (value, expected) in [
            ("gzip", true),
            ("GZIP", true),
            ("br, gzip;q=0.8", true),
            ("deflate, x-gzip", true),
            ("*", true),
            ("br, *;q=0.1", true),
            ("gzip;q=0", false),
            ("gzip; q=0.0, *", false),
            ("*;q=0", false),
            ("br, deflate", false),
            ("identity", false),
            ("", false),
        ] {
            assert_eq!(
                accepts_gzip(&headers("accept-encoding", value)),
                expected,
                "{value:?}"
            );
        }

        assert!(!accepts_gzip(&HeaderMap::new()));
    }

    #[test]
    fn content_encoding_values() {
        assert_eq!(content_encoding(&HeaderMap::new()), Some(Encoding::Identity));
        assert_eq!(
            content_encoding(&headers("content-encoding", "identity")),
            Some(Encoding::Identity)
        );
        assert_eq!(
            content_encoding(&headers("content-encoding", "GZip")),
            Some(Encoding::Gzip)
        );
        assert_eq!(
            content_encoding(&headers("content-encoding", "x-gzip")),
            Some(Encoding::Gzip)
        );
        assert_eq!(content_encoding(&headers("content-encoding", "br")), None);
        assert_eq!(content_encoding(&headers("content-encoding", "gzip, br")), None);
    }

    #[tokio::test]
    async fn request_decoding() {
        let plain = Bytes::from_static(b"{\"hello\":\"world\"}");
        let packed = Bytes::from(gzip(&plain).unwrap());
        let gz = headers("content-encoding", "gzip");

        assert_eq!(
            decode_request_body(&HeaderMap::new(), plain.clone(), 1024)
                .await
                .unwrap(),
            plain
        );
        assert_eq!(decode_request_body(&gz, packed, 1024).await.unwrap(), plain);
        assert!(matches!(
            decode_request_body(&gz, plain.clone(), 1024).await,
            Err(RelayError::BodyCorrupt)
        ));
        assert!(matches!(
            decode_request_body(&headers("content-encoding", "zstd"), plain, 1024).await,
            Err(RelayError::EncodingUnsupported)
        ));
    }

    #[tokio::test]
    async fn truncated_gzip_is_corrupt() {
        let packed = gzip(&vec![b'x'; 10_000]).unwrap();
        let cut = Bytes::copy_from_slice(&packed[..packed.len() / 2]);
        let result = decode_request_body(&headers("content-encoding", "gzip"), cut, 1 << 20).await;

        assert!(matches!(result, Err(RelayError::BodyCorrupt)));
    }

    #[tokio::test]
    async fn bomb_is_cut_off_at_the_cap() {
        let bomb = Bytes::from(gzip(&vec![0u8; 8 * 1024 * 1024]).unwrap());

        assert!(bomb.len() < 64 * 1024);

        let result = decode_request_body(&headers("content-encoding", "gzip"), bomb, 256 * 1024).await;

        assert!(matches!(result, Err(RelayError::BodyTooLarge)));
    }

    #[test]
    fn every_pushed_chunk_is_decodable_as_a_prefix() {
        let chunks: [&[u8]; 3] = [
            b"data: {\"i\":0}\n\n",
            b"data: {\"i\":1}\n\n",
            b"data: [DONE]\n\n",
        ];
        let mut encoder = GzipChunkEncoder::default();
        let mut decoder = GzDecoder::new(Vec::new());
        let mut expected = Vec::new();

        for chunk in chunks {
            let packed = encoder.push(chunk).unwrap();

            assert!(!packed.is_empty());

            decoder.write_all(&packed).unwrap();
            decoder.flush().unwrap();
            expected.extend_from_slice(chunk);

            assert_eq!(
                decoder.get_ref(),
                &expected,
                "chunk not available right after it was pushed"
            );
        }

        decoder.write_all(&encoder.finish().unwrap()).unwrap();

        assert_eq!(decoder.finish().unwrap(), expected);
    }

    #[tokio::test]
    async fn stream_round_trip() {
        let parts = vec![
            Bytes::from_static(b"one "),
            Bytes::new(),
            Bytes::from_static(b"two "),
            Bytes::from_static(b"three"),
        ];
        let source = stream::iter(parts.into_iter().map(Ok::<_, RelayError>));
        let packed: Vec<Bytes> = gzip_stream(source).map(Result::unwrap).collect().await;
        let whole: Vec<u8> = packed.iter().flat_map(|b| b.to_vec()).collect();

        assert_eq!(gunzip(&whole), b"one two three");

        let unpacked: Vec<Bytes> = gunzip_stream(stream::iter(packed.into_iter().map(Ok::<_, RelayError>)))
            .map(Result::unwrap)
            .collect()
            .await;
        let whole: Vec<u8> = unpacked.iter().flat_map(|b| b.to_vec()).collect();

        assert_eq!(whole, b"one two three");
    }

    #[tokio::test]
    async fn stream_errors_end_the_output() {
        let source = stream::iter(vec![
            Ok(Bytes::from_static(b"a")),
            Err(RelayError::UpstreamTimeout),
            Ok(Bytes::from_static(b"b")),
        ]);
        let out: Vec<Result<Bytes, RelayError>> = gzip_stream(source).collect().await;

        assert_eq!(out.len(), 2);
        assert!(out[0].is_ok());
        assert!(matches!(out[1], Err(RelayError::UpstreamTimeout)));

        let garbage = stream::iter(vec![Ok::<_, RelayError>(Bytes::from_static(
            b"definitely not gzip data",
        ))]);
        let out: Vec<Result<Bytes, RelayError>> = gunzip_stream(garbage).collect().await;

        assert!(matches!(
            out.last(),
            Some(Err(RelayError::UpstreamUnreachable(_)))
        ));
    }
}
