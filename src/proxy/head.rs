//! Bounded reading and rewriting of HTTP/1.1 message heads.

use tokio::io::{AsyncRead, AsyncReadExt};

/// Largest request or response head accepted, in bytes.
pub(super) const MAX_HEAD_BYTES: usize = 32 * 1024;
const MAX_HEADERS: usize = 100;

/// A message head read from a stream plus any bytes that followed it.
pub(super) struct Head {
    pub(super) head: Vec<u8>,
    pub(super) rest: Vec<u8>,
}

/// Reads until the blank line that ends a head. `None` on a clean close before
/// any byte; an error when the head is too large or the stream ends midway.
pub(super) async fn read_head<R: AsyncRead + Unpin>(
    reader: &mut R,
    mut buf: Vec<u8>,
) -> std::io::Result<Option<Head>> {
    loop {
        if let Some(end) = find_head_end(&buf) {
            let rest = buf.split_off(end);
            return Ok(Some(Head { head: buf, rest }));
        }
        if buf.len() >= MAX_HEAD_BYTES {
            return Err(std::io::Error::other("message head too large"));
        }
        let mut chunk = [0u8; 4096];
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            if buf.is_empty() {
                return Ok(None);
            }
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    memchr::memmem::find(buf, b"\r\n\r\n").map(|i| i + 4)
}

/// What the relay was asked to do.
pub(super) enum Request {
    /// `CONNECT host:port`.
    Connect { host: String, port: u16 },
    /// An absolute-form request (`GET http://host/path HTTP/1.1`).
    Forward {
        host: String,
        port: u16,
        method: String,
        absolute: String,
        origin_form: String,
        headers: Vec<(String, Vec<u8>)>,
    },
}

/// Parses a request head. Origin-form and every other shape is refused, so a
/// web page (which can only send origin-form) cannot drive the relay.
pub(super) fn parse_request(head: &[u8]) -> Result<Request, &'static str> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut req = httparse::Request::new(&mut headers);
    match req.parse(head) {
        Ok(httparse::Status::Complete(_)) => {}
        _ => return Err("malformed request"),
    }
    let method = req.method.ok_or("malformed request")?;
    let target = req.path.ok_or("malformed request")?;
    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = split_authority(target, None).ok_or("bad CONNECT target")?;
        return Ok(Request::Connect { host, port });
    }
    let rest = if target.len() > 7 && target[..7].eq_ignore_ascii_case("http://") {
        &target[7..]
    } else {
        return Err("only proxy requests are accepted");
    };
    let (authority, path) = match rest.find(['/', '?']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = split_authority(authority, Some(80)).ok_or("bad request target")?;
    let origin_form = if path.starts_with('?') {
        format!("/{path}")
    } else {
        path.to_string()
    };
    Ok(Request::Forward {
        host,
        port,
        method: method.to_string(),
        absolute: target.to_string(),
        origin_form,
        headers: req
            .headers
            .iter()
            .map(|h| (h.name.to_string(), h.value.to_vec()))
            .collect(),
    })
}

fn split_authority(s: &str, default_port: Option<u16>) -> Option<(String, u16)> {
    if s.contains('@') || s.is_empty() {
        return None;
    }
    let (host, port) = if let Some(inner) = s.strip_prefix('[') {
        let (host, after) = inner.split_once(']')?;
        (host, after.strip_prefix(':'))
    } else {
        match s.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (s, None),
        }
    };
    let port = match port {
        Some(p) => p.parse().ok().filter(|p: &u16| *p != 0)?,
        None => default_port?,
    };
    (!host.is_empty()).then(|| (host.to_string(), port))
}

/// Headers that belong to one hop and never travel further, plus any header a
/// client names in `Connection:`.
fn hop_by_hop(headers: &[(String, Vec<u8>)]) -> Vec<String> {
    let mut names: Vec<String> = [
        "connection",
        "keep-alive",
        "proxy-authorization",
        "proxy-authenticate",
        "proxy-connection",
        "te",
        "trailer",
        "upgrade",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("connection") {
            for token in String::from_utf8_lossy(value).split(',') {
                let token = token.trim().to_ascii_lowercase();
                if !token.is_empty() {
                    names.push(token);
                }
            }
        }
    }
    names
}

/// Rebuilds a forwarded request head: hop-by-hop headers dropped,
/// `Connection: close` set, and `Proxy-Authorization` added when given.
pub(super) fn rewrite_request(
    method: &str,
    target: &str,
    headers: &[(String, Vec<u8>)],
    proxy_auth: Option<&str>,
) -> Vec<u8> {
    let drop = hop_by_hop(headers);
    let mut out = Vec::with_capacity(512);
    out.extend_from_slice(format!("{method} {target} HTTP/1.1\r\n").as_bytes());
    for (name, value) in headers {
        if drop.iter().any(|d| name.eq_ignore_ascii_case(d)) {
            continue;
        }
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value);
        out.extend_from_slice(b"\r\n");
    }
    if let Some(auth) = proxy_auth {
        out.extend_from_slice(b"Proxy-Authorization: ");
        out.extend_from_slice(auth.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    out
}

/// Status code of a response head, if it parses.
pub(super) fn response_status(head: &[u8]) -> Option<u16> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut res = httparse::Response::new(&mut headers);
    match res.parse(head) {
        Ok(httparse::Status::Complete(_)) => res.code,
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_connect_and_absolute_form_and_refuses_origin_form() {
        assert!(matches!(
            parse_request(b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n"),
            Ok(Request::Connect { ref host, port: 443 }) if host == "example.com"
        ));
        match parse_request(
            b"GET http://example.com:8080/a?b HTTP/1.1\r\nHost: example.com\r\n\r\n",
        ) {
            Ok(Request::Forward {
                host,
                port,
                origin_form,
                ..
            }) => {
                assert_eq!(
                    (host.as_str(), port, origin_form.as_str()),
                    ("example.com", 8080, "/a?b")
                );
            }
            _ => panic!("expected forward"),
        }
        match parse_request(b"GET http://example.com?q HTTP/1.1\r\n\r\n") {
            Ok(Request::Forward {
                port, origin_form, ..
            }) => assert_eq!((port, origin_form.as_str()), (80, "/?q")),
            _ => panic!("expected forward"),
        }
        assert!(parse_request(b"GET / HTTP/1.1\r\nHost: 127.0.0.1:5555\r\n\r\n").is_err());
        assert!(parse_request(b"GET https://example.com/ HTTP/1.1\r\n\r\n").is_err());
        assert!(parse_request(b"CONNECT example.com HTTP/1.1\r\n\r\n").is_err());
        assert!(parse_request(b"CONNECT user@example.com:443 HTTP/1.1\r\n\r\n").is_err());
    }

    #[test]
    fn rewrite_drops_hop_by_hop_and_adds_login_and_close() {
        let headers = vec![
            ("Host".to_string(), b"example.com".to_vec()),
            (
                "Proxy-Authorization".to_string(),
                b"Basic Y2xpZW50".to_vec(),
            ),
            ("Connection".to_string(), b"keep-alive, X-Secret".to_vec()),
            ("X-Secret".to_string(), b"1".to_vec()),
            ("Accept".to_string(), b"*/*".to_vec()),
        ];
        let out = String::from_utf8(rewrite_request(
            "GET",
            "http://example.com/",
            &headers,
            Some("Basic dXA="),
        ))
        .unwrap();
        assert_eq!(
            out,
            "GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\nAccept: */*\r\nProxy-Authorization: Basic dXA=\r\nConnection: close\r\n\r\n"
        );
    }

    #[tokio::test]
    async fn read_head_keeps_trailing_bytes_and_bounds_size() {
        let mut input: &[u8] = b"HTTP/1.1 200 OK\r\n\r\nhello";
        let head = read_head(&mut input, Vec::new()).await.unwrap().unwrap();
        assert_eq!(head.rest, b"hello");
        assert_eq!(response_status(&head.head), Some(200));
        let big = vec![b'a'; MAX_HEAD_BYTES + 10];
        let mut input: &[u8] = &big;
        assert!(read_head(&mut input, Vec::new()).await.is_err());
        let mut empty: &[u8] = b"";
        assert!(read_head(&mut empty, Vec::new()).await.unwrap().is_none());
    }
}
