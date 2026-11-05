//! Token authentication: query param or bearer header, constant-time compare.

use subtle::ConstantTimeEq;

/// Checks a request's credentials against the configured token.
///
/// `None` config means auth is disabled. The candidate is the `token` query
/// parameter when present, otherwise a `Bearer` authorization header.
/// Comparison is constant-time in the token contents.
#[must_use]
pub fn authorized(
    configured: Option<&str>,
    query_token: Option<&str>,
    authorization_header: Option<&str>,
) -> bool {
    let Some(expected) = configured else {
        return true;
    };
    let candidate = query_token.or_else(|| {
        authorization_header
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(str::trim)
    });
    let Some(candidate) = candidate else {
        return false;
    };
    expected.as_bytes().ct_eq(candidate.as_bytes()).into()
}

/// Whether a browser `Origin` may reach an instance that runs without a token.
///
/// CDP clients do not send `Origin`, so an absent header is allowed. A browser
/// always sends one on a WebSocket upgrade, and a page on another site (or a
/// rebound DNS name) must not get a browser from a tokenless instance, so only
/// loopback origins pass.
#[must_use]
pub fn tokenless_origin_allowed(origin: Option<&str>) -> bool {
    let Some(origin) = origin else {
        return true;
    };
    let Some((_, rest)) = origin.split_once("://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or_default();
    let host = if let Some(stripped) = authority.strip_prefix('[') {
        stripped.split(']').next().unwrap_or_default()
    } else {
        authority.rsplit_once(':').map_or(authority, |(h, _)| h)
    };
    let host = host.to_ascii_lowercase();
    host == "localhost"
        || host == "::1"
        || host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenless_origin_allows_cdp_clients_and_loopback_pages_only() {
        assert!(tokenless_origin_allowed(None));
        assert!(tokenless_origin_allowed(Some("http://localhost:3000")));
        assert!(tokenless_origin_allowed(Some("http://127.0.0.1")));
        assert!(tokenless_origin_allowed(Some("http://[::1]:9222")));
        assert!(!tokenless_origin_allowed(Some("https://attacker.example")));
        assert!(!tokenless_origin_allowed(Some(
            "http://localhost.attacker.example"
        )));
        assert!(!tokenless_origin_allowed(Some("http://127.0.0.1.nip.io")));
        assert!(!tokenless_origin_allowed(Some("null")));
    }

    #[test]
    fn no_configured_token_allows_everything() {
        assert!(authorized(None, None, None));
        assert!(authorized(None, Some("anything"), None));
    }

    #[test]
    fn query_token_must_match_exactly() {
        assert!(authorized(Some("secret"), Some("secret"), None));
        assert!(!authorized(Some("secret"), Some("secre"), None));
        assert!(!authorized(Some("secret"), Some("secretx"), None));
        assert!(!authorized(Some("secret"), Some(""), None));
    }

    #[test]
    fn bearer_header_works_when_no_query_token() {
        assert!(authorized(Some("secret"), None, Some("Bearer secret")));
        assert!(authorized(Some("secret"), None, Some("Bearer secret ")));
        assert!(!authorized(Some("secret"), None, Some("Bearer wrong")));
        assert!(!authorized(Some("secret"), None, Some("Basic secret")));
        assert!(!authorized(Some("secret"), None, None));
    }

    #[test]
    fn query_token_takes_precedence_over_header() {
        assert!(!authorized(
            Some("secret"),
            Some("wrong"),
            Some("Bearer secret")
        ));
    }
}
