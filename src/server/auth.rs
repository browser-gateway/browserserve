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

#[cfg(test)]
mod tests {
    use super::*;

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
