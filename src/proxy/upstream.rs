//! Parsing a proxy address such as `http://user:pass@host:8080`.

use std::fmt;
use thiserror::Error;
use zeroize::Zeroizing;

/// How the relay talks to the upstream proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyScheme {
    /// Plain HTTP proxy (`CONNECT` and absolute-form requests).
    Http,
    /// HTTP proxy reached over TLS.
    Https,
    /// SOCKS5 (`socks5://` or `socks5h://`); the proxy always resolves target host names.
    Socks5,
}

impl ProxyScheme {
    fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Socks5 => "socks5",
        }
    }

    fn default_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https => 443,
            Self::Socks5 => 1080,
        }
    }
}

/// Why a proxy address was refused.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum UpstreamParseError {
    /// The address does not start with `http://`, `https://`, `socks5://` or `socks5h://`.
    #[error("proxy address must start with http://, https://, socks5:// or socks5h://")]
    Scheme,
    /// Host or port missing or malformed.
    #[error("proxy address has no valid host and port")]
    HostPort,
    /// A path, query or fragment follows the host.
    #[error("proxy address must not contain a path")]
    Path,
    /// The login is malformed or too long for the scheme.
    #[error("proxy login is not valid: {0}")]
    Login(&'static str),
}

/// A parsed upstream proxy. `Debug` and `Display` never show the login.
#[derive(Clone)]
pub struct ProxyUpstream {
    scheme: ProxyScheme,
    host: String,
    port: u16,
    username: Option<Zeroizing<String>>,
    password: Option<Zeroizing<String>>,
}

impl ProxyUpstream {
    /// Parses `scheme://[user[:pass]@]host[:port][/]`. The username and
    /// password are percent-decoded exactly once.
    ///
    /// # Errors
    ///
    /// Any [`UpstreamParseError`] for an address the relay cannot use.
    pub fn parse(raw: &str) -> Result<Self, UpstreamParseError> {
        let raw = raw.trim();
        let (scheme, rest) = if let Some(r) = strip_prefix_ci(raw, "http://") {
            (ProxyScheme::Http, r)
        } else if let Some(r) = strip_prefix_ci(raw, "https://") {
            (ProxyScheme::Https, r)
        } else if let Some(r) =
            strip_prefix_ci(raw, "socks5://").or_else(|| strip_prefix_ci(raw, "socks5h://"))
        {
            (ProxyScheme::Socks5, r)
        } else {
            return Err(UpstreamParseError::Scheme);
        };
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        if rest.contains(['/', '?', '#']) {
            return Err(UpstreamParseError::Path);
        }
        let (login, hostport) = match rest.rfind('@') {
            Some(at) => (Some(&rest[..at]), &rest[at + 1..]),
            None => (None, rest),
        };
        let (host, port) = split_host_port(hostport, scheme.default_port())?;
        let (username, password) = match login {
            None => (None, None),
            Some(login) => {
                let (u, p) = match login.split_once(':') {
                    Some((u, p)) => (u, Some(p)),
                    None => (login, None),
                };
                let user = percent_decode(u).ok_or(UpstreamParseError::Login("bad escape"))?;
                let pass = p
                    .map(|p| percent_decode(p).ok_or(UpstreamParseError::Login("bad escape")))
                    .transpose()?;
                (Some(user), pass)
            }
        };
        validate_login(scheme, username.as_deref(), password.as_deref())?;
        Ok(Self {
            scheme,
            host,
            port,
            username: username.map(Zeroizing::new),
            password: password.map(Zeroizing::new),
        })
    }

    /// The protocol used to reach the proxy.
    #[must_use]
    pub fn scheme(&self) -> ProxyScheme {
        self.scheme
    }

    /// Proxy host name or IP literal (IPv6 without brackets).
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Proxy port.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// True when the address carries a username.
    #[must_use]
    pub fn has_login(&self) -> bool {
        self.username.is_some()
    }

    pub(super) fn username(&self) -> Option<&str> {
        self.username.as_deref().map(String::as_str)
    }

    pub(super) fn password(&self) -> Option<&str> {
        self.password.as_deref().map(String::as_str)
    }

    /// `Basic` credentials for a `Proxy-Authorization` header, if any.
    pub(super) fn basic_auth(&self) -> Option<Zeroizing<String>> {
        use base64::Engine as _;
        let user = self.username()?;
        let pass = self.password().unwrap_or("");
        let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
        Some(Zeroizing::new(format!("Basic {encoded}")))
    }
}

impl fmt::Display for ProxyUpstream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') {
            write!(
                f,
                "{}://[{}]:{}",
                self.scheme.as_str(),
                self.host,
                self.port
            )
        } else {
            write!(f, "{}://{}:{}", self.scheme.as_str(), self.host, self.port)
        }
    }
}

impl fmt::Debug for ProxyUpstream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyUpstream")
            .field("server", &self.to_string())
            .field("has_login", &self.has_login())
            .finish()
    }
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

fn split_host_port(s: &str, default_port: u16) -> Result<(String, u16), UpstreamParseError> {
    let (host, port) = if let Some(inner) = s.strip_prefix('[') {
        let (host, after) = inner.split_once(']').ok_or(UpstreamParseError::HostPort)?;
        let port = match after {
            "" => None,
            p => Some(p.strip_prefix(':').ok_or(UpstreamParseError::HostPort)?),
        };
        (host, port)
    } else {
        match s.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (s, None),
        }
    };
    if host.is_empty()
        || !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '_'))
    {
        return Err(UpstreamParseError::HostPort);
    }
    let port = match port {
        None => default_port,
        Some(p) => p
            .parse::<u16>()
            .ok()
            .filter(|p| *p != 0)
            .ok_or(UpstreamParseError::HostPort)?,
    };
    Ok((host.to_ascii_lowercase(), port))
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn validate_login(
    scheme: ProxyScheme,
    user: Option<&str>,
    pass: Option<&str>,
) -> Result<(), UpstreamParseError> {
    let Some(user) = user else { return Ok(()) };
    if user.is_empty() {
        return Err(UpstreamParseError::Login("empty username"));
    }
    let bad_char = |s: &str| s.chars().any(char::is_control);
    if bad_char(user) || pass.is_some_and(bad_char) {
        return Err(UpstreamParseError::Login("control character"));
    }
    match scheme {
        ProxyScheme::Http | ProxyScheme::Https if user.contains(':') => {
            Err(UpstreamParseError::Login("username contains ':'"))
        }
        ProxyScheme::Socks5 if user.len() > 255 || pass.is_some_and(|p| p.len() > 255) => {
            Err(UpstreamParseError::Login("longer than 255 bytes"))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_scheme_with_default_ports() {
        let p = ProxyUpstream::parse("http://proxy.example.com").unwrap();
        assert_eq!(
            (p.scheme(), p.host(), p.port()),
            (ProxyScheme::Http, "proxy.example.com", 80)
        );
        let p = ProxyUpstream::parse("HTTPS://Proxy.Example.com:8443/").unwrap();
        assert_eq!(
            (p.scheme(), p.host(), p.port()),
            (ProxyScheme::Https, "proxy.example.com", 8443)
        );
        let p = ProxyUpstream::parse("socks5://[2001:db8::1]:1081").unwrap();
        assert_eq!(
            (p.scheme(), p.host(), p.port()),
            (ProxyScheme::Socks5, "2001:db8::1", 1081)
        );
        assert_eq!(p.to_string(), "socks5://[2001:db8::1]:1081");
        let p = ProxyUpstream::parse("socks5h://u:p@h.example.com").unwrap();
        assert_eq!((p.scheme(), p.port()), (ProxyScheme::Socks5, 1080));
    }

    #[test]
    fn decodes_the_login_once_and_never_shows_it() {
        let p = ProxyUpstream::parse("http://al%40ice:p%3Ass%2525@h.example.com:1").unwrap();
        assert_eq!(p.username(), Some("al@ice"));
        assert_eq!(p.password(), Some("p:ss%25"));
        assert!(!format!("{p:?} {p}").contains("ss"));
        assert!(!format!("{p:?}").contains("al"));
        assert_eq!(
            p.basic_auth().unwrap().as_str(),
            "Basic YWxAaWNlOnA6c3MlMjU="
        );
    }

    #[test]
    fn password_with_at_sign_splits_on_the_last_at() {
        let p = ProxyUpstream::parse("http://u:p@ss@h.example.com:1").unwrap();
        assert_eq!(p.password(), Some("p@ss"));
    }

    #[test]
    fn refuses_bad_addresses() {
        use UpstreamParseError as E;
        assert_eq!(ProxyUpstream::parse("ftp://h:1").unwrap_err(), E::Scheme);
        assert_eq!(ProxyUpstream::parse("socks4://h:1").unwrap_err(), E::Scheme);
        assert_eq!(
            ProxyUpstream::parse("http://h:1/path").unwrap_err(),
            E::Path
        );
        assert_eq!(ProxyUpstream::parse("http://h:0").unwrap_err(), E::HostPort);
        assert_eq!(
            ProxyUpstream::parse("http://h:99999").unwrap_err(),
            E::HostPort
        );
        assert_eq!(
            ProxyUpstream::parse("http://:8080").unwrap_err(),
            E::HostPort
        );
        assert_eq!(
            ProxyUpstream::parse("http://h h:1").unwrap_err(),
            E::HostPort
        );
        assert!(matches!(
            ProxyUpstream::parse("http://a%3Ab:p@h:1"),
            Err(E::Login(_))
        ));
        assert!(matches!(
            ProxyUpstream::parse("http://%zz@h:1"),
            Err(E::Login(_))
        ));
        assert!(matches!(
            ProxyUpstream::parse("http://u%0A:p@h:1"),
            Err(E::Login(_))
        ));
        let long = "x".repeat(256);
        assert!(matches!(
            ProxyUpstream::parse(&format!("socks5://{long}:p@h:1")),
            Err(E::Login(_))
        ));
        assert!(ProxyUpstream::parse(&format!("http://{long}:p@h:1")).is_ok());
    }
}
