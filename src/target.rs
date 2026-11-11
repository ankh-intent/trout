use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use axum::http::HeaderMap;
use reqwest::Url;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};

use crate::config::{Config, TargetMode};
use crate::error::RelayError;

pub const TARGET_HEADER: &str = "x-trout-target";

/// A target that passed every check that can be made without touching the network.
#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    /// Normalised base URL without a trailing slash, e.g. `https://api.example.com/v1`.
    pub base_url: String,
    pub host: String,
    pub accepts_gzip: bool,
    pub allow_private: bool,
}

/// Parses a provider base URL and brings it to the canonical form used for comparison.
pub fn normalise_base_url(raw: &str) -> Result<Url, String> {
    let mut url = Url::parse(raw.trim()).map_err(|_| "not an absolute URL".to_string())?;

    if !matches!(url.scheme(), "https" | "http") {
        return Err("scheme must be https".to_string());
    }

    if url.host().is_none() {
        return Err("host is missing".to_string());
    }

    if !url.username().is_empty() || url.password().is_some() {
        return Err("user-info is not allowed".to_string());
    }

    if url.query().is_some() {
        return Err("query is not allowed".to_string());
    }

    if url.fragment().is_some() {
        return Err("fragment is not allowed".to_string());
    }

    let path = url.path().trim_end_matches('/').to_string();

    url.set_path(&path);

    Ok(url)
}

/// Reads `X-Trout-Target` and applies the operator's target rules.
pub fn resolve(headers: &HeaderMap, config: &Config) -> Result<ResolvedTarget, RelayError> {
    let raw = headers.get(TARGET_HEADER).ok_or(RelayError::TargetMissing)?;
    let raw = raw
        .to_str()
        .map_err(|_| RelayError::TargetInvalid("not valid text".to_string()))?;
    let url = normalise_base_url(raw).map_err(RelayError::TargetInvalid)?;
    let base_url = url.as_str().trim_end_matches('/').to_string();
    let entry = config.targets.list.iter().find(|e| e.base_url == base_url);

    if entry.is_none() && config.targets.mode == TargetMode::Listed {
        return Err(RelayError::TargetForbidden(
            "not in the configured list".to_string(),
        ));
    }

    let accepts_gzip = entry.is_some_and(|e| e.accepts_gzip);
    let allow_private = entry.is_some_and(|e| e.allow_private);

    if url.scheme() == "http" && !allow_private {
        return Err(RelayError::TargetInvalid(
            "plain http is only allowed for a listed target with allow_private".to_string(),
        ));
    }

    let host = url.host_str().unwrap_or_default();
    let literal_ip = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .ok();

    if let Some(ip) = literal_ip
        && !allow_private
        && !is_public(ip)
    {
        return Err(RelayError::TargetForbidden("address is not public".to_string()));
    }

    Ok(ResolvedTarget {
        base_url,
        host: host.to_string(),
        accepts_gzip,
        allow_private,
    })
}

pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_public_v4(v4),
            None => is_public_v6(v6),
        },
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();

    !(a == 0
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || (a == 100 && (64..128).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 198 && (b == 18 || b == 19))
        || a >= 240)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let first = ip.segments()[0];

    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (first & 0xfe00) == 0xfc00
        || (first & 0xffc0) == 0xfe80
        || (first == 0x2001 && ip.segments()[1] == 0x0db8))
}

/// Raised by [`PublicOnlyResolver`]; found again in the client's error chain to answer
/// `target_forbidden` instead of `upstream_unreachable`.
#[derive(Debug, thiserror::Error)]
#[error("{NON_PUBLIC_MARKER}: {0}")]
pub struct NonPublicAddress(pub String);

pub const NON_PUBLIC_MARKER: &str = "trout: host resolves only to non-public addresses";

/// DNS resolver for the outbound client that discards non-public addresses, so the check applies
/// to the address actually connected to.
#[derive(Debug, Default, Clone)]
pub struct PublicOnlyResolver;

impl Resolve for PublicOnlyResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();

        Box::pin(async move {
            let resolved = tokio::net::lookup_host((host.as_str(), 0)).await?;
            let public: Vec<SocketAddr> = resolved.filter(|addr| is_public(addr.ip())).collect();

            if public.is_empty() {
                return Err(Box::new(NonPublicAddress(host)) as Box<dyn std::error::Error + Send + Sync>);
            }

            Ok(Box::new(public.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TargetEntry;

    fn config(mode: TargetMode, list: Vec<TargetEntry>) -> Config {
        let mut config = Config {
            access_key: "k".repeat(32),
            ..Config::default()
        };

        config.targets.mode = mode;
        config.targets.list = list;
        config.validate().unwrap();

        config
    }

    fn headers(target: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();

        headers.insert(TARGET_HEADER, target.parse().unwrap());

        headers
    }

    fn entry(base_url: &str, accepts_gzip: bool, allow_private: bool) -> TargetEntry {
        TargetEntry {
            base_url: base_url.to_string(),
            accepts_gzip,
            allow_private,
        }
    }

    #[test]
    fn normalises_case_default_port_and_trailing_slash() {
        let url = normalise_base_url("HTTPS://API.Example.com:443/v1/").unwrap();

        assert_eq!(url.as_str(), "https://api.example.com/v1");
        assert_eq!(
            normalise_base_url("https://api.example.com").unwrap().as_str(),
            "https://api.example.com/"
        );
    }

    #[test]
    fn rejects_unusable_urls() {
        for raw in [
            "/v1",
            "api.example.com/v1",
            "ftp://api.example.com/v1",
            "https://user:pw@api.example.com/v1",
            "https://api.example.com/v1?key=1",
            "https://api.example.com/v1#frag",
        ] {
            assert!(normalise_base_url(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn missing_header() {
        let result = resolve(&HeaderMap::new(), &config(TargetMode::AnyPublic, vec![]));

        assert!(matches!(result, Err(RelayError::TargetMissing)));
    }

    #[test]
    fn any_public_accepts_unlisted_https() {
        let target = resolve(
            &headers("https://api.example.com/v1/"),
            &config(TargetMode::AnyPublic, vec![]),
        )
        .unwrap();

        assert_eq!(target.base_url, "https://api.example.com/v1");
        assert_eq!(target.host, "api.example.com");
        assert!(!target.accepts_gzip);
        assert!(!target.allow_private);
    }

    #[test]
    fn listed_mode_refuses_unlisted() {
        let config = config(
            TargetMode::Listed,
            vec![entry("https://api.example.com/v1", true, false)],
        );

        assert!(matches!(
            resolve(&headers("https://other.example.com/v1"), &config),
            Err(RelayError::TargetForbidden(_))
        ));

        let listed = resolve(&headers("https://API.example.com/v1/"), &config).unwrap();

        assert!(listed.accepts_gzip);
    }

    #[test]
    fn plain_http_needs_a_private_listing() {
        let open = config(TargetMode::AnyPublic, vec![]);
        let listed = config(
            TargetMode::AnyPublic,
            vec![entry("http://127.0.0.1:9000/v1", false, true)],
        );

        assert!(matches!(
            resolve(&headers("http://api.example.com/v1"), &open),
            Err(RelayError::TargetInvalid(_))
        ));
        assert!(
            resolve(&headers("http://127.0.0.1:9000/v1"), &listed)
                .unwrap()
                .allow_private
        );
    }

    #[test]
    fn literal_non_public_addresses_are_forbidden() {
        let open = config(TargetMode::AnyPublic, vec![]);

        for raw in [
            "https://127.0.0.1:9/v1",
            "https://10.1.2.3/v1",
            "https://192.168.1.1/v1",
            "https://169.254.169.254/v1",
            "https://[::1]/v1",
            "https://[fd00::1]/v1",
        ] {
            assert!(
                matches!(resolve(&headers(raw), &open), Err(RelayError::TargetForbidden(_))),
                "{raw}"
            );
        }
    }

    #[test]
    fn address_ranges() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.5.4",
            "192.168.0.1",
            "169.254.1.1",
            "100.64.0.1",
            "100.127.255.254",
            "0.0.0.0",
            "198.18.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fc00::1",
            "fdff::1",
            "fe80::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip} should not be public");
        }

        for ip in [
            "1.1.1.1",
            "8.8.8.8",
            "100.63.255.255",
            "100.128.0.1",
            "2606:4700:4700::1111",
            "::ffff:8.8.8.8",
        ] {
            assert!(is_public(ip.parse().unwrap()), "{ip} should be public");
        }
    }
}
