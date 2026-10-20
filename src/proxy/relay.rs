//! The local relay Chrome uses as its proxy for one session.

use super::ProxyError;
use super::dial::{Dialer, UpstreamStream};
use super::head::{Request, parse_request, read_head, response_status, rewrite_request};
use super::upstream::{ProxyScheme, ProxyUpstream};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, copy_bidirectional};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

/// Relay limits.
#[derive(Debug, Clone)]
pub struct RelayOptions {
    /// Allow an upstream proxy on a private or local network.
    pub allow_private: bool,
    /// Limit for connecting and negotiating with the upstream proxy.
    pub connect_timeout: Duration,
    /// Simultaneous connections Chrome may hold open through the relay.
    pub max_connections: usize,
}

impl Default for RelayOptions {
    fn default() -> Self {
        Self {
            allow_private: false,
            connect_timeout: Duration::from_secs(20),
            max_connections: 256,
        }
    }
}

/// A running relay. Dropping it stops the listener and closes every
/// connection that passes through it.
#[derive(Debug)]
pub struct Relay {
    port: u16,
    cancel: CancellationToken,
}

impl Relay {
    /// Checks the upstream address and starts listening on `127.0.0.1`.
    ///
    /// # Errors
    ///
    /// [`ProxyError::AddressNotAllowed`] for a private or unresolvable proxy
    /// host, [`ProxyError::Unreachable`] when the listener cannot bind or TLS
    /// is unavailable.
    pub async fn start(upstream: ProxyUpstream, options: RelayOptions) -> Result<Self, ProxyError> {
        let dialer =
            Arc::new(Dialer::new(upstream, options.allow_private, options.connect_timeout).await?);
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| ProxyError::Unreachable(format!("relay bind: {}", e.kind())))?;
        let port = listener
            .local_addr()
            .map_err(|e| ProxyError::Unreachable(format!("relay bind: {}", e.kind())))?
            .port();
        let cancel = CancellationToken::new();
        tokio::spawn(accept_loop(
            listener,
            dialer,
            cancel.clone(),
            options.max_connections,
        ));
        Ok(Self { port, cancel })
    }

    /// The local port Chrome connects to.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Value for Chrome's `--proxy-server` switch.
    #[must_use]
    pub fn proxy_server(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn accept_loop(
    listener: TcpListener,
    dialer: Arc<Dialer>,
    cancel: CancellationToken,
    max: usize,
) {
    let slots = Arc::new(Semaphore::new(max));
    loop {
        let accepted = tokio::select! {
            () = cancel.cancelled() => return,
            accepted = listener.accept() => accepted,
        };
        let Ok((client, peer)) = accepted else {
            continue;
        };
        if !peer.ip().is_loopback() {
            continue;
        }
        let Ok(permit) = Arc::clone(&slots).try_acquire_owned() else {
            drop(client);
            continue;
        };
        let dialer = Arc::clone(&dialer);
        let cancel = cancel.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = cancel.cancelled() => {}
                () = serve(client, &dialer) => {}
            }
            drop(permit);
        });
    }
}

async fn serve(mut client: TcpStream, dialer: &Dialer) {
    let _ = client.set_nodelay(true);
    let head = match read_head(&mut client, Vec::new()).await {
        Ok(Some(head)) => head,
        Ok(None) => return,
        Err(_) => return reply(&mut client, 400, "bad request").await,
    };
    let request = match parse_request(&head.head) {
        Ok(request) => request,
        Err(reason) => return reply(&mut client, 400, reason).await,
    };
    let outcome = match request {
        Request::Connect { host, port } => {
            connect(&mut client, dialer, &host, port, head.rest).await
        }
        Request::Forward {
            host,
            port,
            method,
            absolute,
            origin_form,
            headers,
        } => {
            let opened = match dialer.upstream().scheme() {
                ProxyScheme::Socks5 => dialer.tunnel(&host, port).await.map(|(s, rest)| {
                    (
                        s,
                        rest,
                        rewrite_request(&method, &origin_form, &headers, None),
                    )
                }),
                ProxyScheme::Http | ProxyScheme::Https => {
                    let auth = dialer.upstream().basic_auth();
                    dialer.open().await.map(|s| {
                        (
                            s,
                            Vec::new(),
                            rewrite_request(
                                &method,
                                &absolute,
                                &headers,
                                auth.as_deref().map(String::as_str),
                            ),
                        )
                    })
                }
            };
            match opened {
                Ok((upstream, rest, request_head)) => {
                    forward(&mut client, upstream, rest, &request_head, &head.rest).await
                }
                Err(e) => Err(e),
            }
        }
    };
    if let Err(e) = outcome {
        tracing::debug!(error = %e, "proxy relay: connection failed");
        reply(&mut client, 502, &e.to_string()).await;
    }
}

async fn connect(
    client: &mut TcpStream,
    dialer: &Dialer,
    host: &str,
    port: u16,
    early: Vec<u8>,
) -> Result<(), ProxyError> {
    let (mut upstream, rest) = dialer.tunnel(host, port).await?;
    if client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await
        .is_err()
        || client.write_all(&rest).await.is_err()
        || upstream.write_all(&early).await.is_err()
    {
        return Ok(());
    }
    let _ = copy_bidirectional(client, &mut upstream).await;
    Ok(())
}

async fn forward(
    client: &mut TcpStream,
    mut upstream: UpstreamStream,
    tunnel_rest: Vec<u8>,
    request_head: &[u8],
    early_body: &[u8],
) -> Result<(), ProxyError> {
    let io = |e: std::io::Error| ProxyError::Unreachable(e.kind().to_string());
    upstream.write_all(request_head).await.map_err(io)?;
    upstream.write_all(early_body).await.map_err(io)?;
    let (mut client_read, mut client_write) = client.split();
    let (mut up_read, mut up_write) = tokio::io::split(upstream);
    let body = async {
        let _ = tokio::io::copy(&mut client_read, &mut up_write).await;
        std::future::pending::<()>().await;
    };
    let response = async {
        let head = read_head(&mut up_read, tunnel_rest)
            .await
            .map_err(io)?
            .ok_or_else(|| ProxyError::Unreachable("proxy closed the connection".into()))?;
        if matches!(response_status(&head.head), Some(401 | 407)) {
            return Err(ProxyError::LoginFailed);
        }
        client_write.write_all(&head.head).await.map_err(io)?;
        client_write.write_all(&head.rest).await.map_err(io)?;
        let _ = tokio::io::copy(&mut up_read, &mut client_write).await;
        let _ = client_write.shutdown().await;
        Ok(())
    };
    tokio::select! {
        () = body => Ok(()),
        result = response => result,
    }
}

async fn reply(client: &mut TcpStream, status: u16, reason: &str) {
    let text = match status {
        400 => "Bad Request",
        _ => "Bad Gateway",
    };
    let body = format!("{reason}\n");
    let msg = format!(
        "HTTP/1.1 {status} {text}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = client.write_all(msg.as_bytes()).await;
    let _ = client.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::sync::mpsc;

    const AUTH: &str = "Basic dTpw";

    /// A fake HTTP proxy: answers CONNECT with 200 (then echoes) and absolute
    /// requests with a fixed body; 407 when the login is missing or wrong.
    /// Every request head it receives is sent on the channel.
    async fn fake_http_proxy() -> (u16, mpsc::UnboundedReceiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                let tx = tx.clone();
                tokio::spawn(async move {
                    let Ok(Some(head)) = read_head(&mut s, Vec::new()).await else {
                        return;
                    };
                    let text = String::from_utf8_lossy(&head.head).to_string();
                    let _ = tx.send(text.clone());
                    if !text.contains(&format!("Proxy-Authorization: {AUTH}\r\n")) {
                        let _ = s.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic\r\nContent-Length: 0\r\n\r\n").await;
                        return;
                    }
                    if text.starts_with("CONNECT") {
                        let _ = s.write_all(b"HTTP/1.1 200 OK\r\n\r\nfrom-proxy;").await;
                        let mut buf = head.rest;
                        let mut chunk = [0u8; 1024];
                        loop {
                            if !buf.is_empty() {
                                let _ = s.write_all(&buf).await;
                                buf.clear();
                            }
                            match s.read(&mut chunk).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                            }
                        }
                    }
                    let _ = s
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                        .await;
                });
            }
        });
        (port, rx)
    }

    async fn relay_to(upstream: &str) -> Relay {
        let options = RelayOptions {
            allow_private: true,
            ..RelayOptions::default()
        };
        Relay::start(ProxyUpstream::parse(upstream).unwrap(), options)
            .await
            .unwrap()
    }

    async fn send(relay: &Relay, bytes: &[u8]) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", relay.port()))
            .await
            .unwrap();
        s.write_all(bytes).await.unwrap();
        let mut out = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            match tokio::time::timeout(Duration::from_millis(500), s.read(&mut chunk)).await {
                Ok(Ok(n)) if n > 0 => out.extend_from_slice(&chunk[..n]),
                _ => break,
            }
        }
        String::from_utf8_lossy(&out).to_string()
    }

    #[tokio::test]
    async fn connect_adds_the_login_and_keeps_bytes_on_both_sides() {
        let (port, mut seen) = fake_http_proxy().await;
        let relay = relay_to(&format!("http://u:p@127.0.0.1:{port}")).await;
        let out = send(
            &relay,
            b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\nearly",
        )
        .await;
        assert_eq!(
            out,
            "HTTP/1.1 200 Connection Established\r\n\r\nfrom-proxy;early"
        );
        let head = seen.recv().await.unwrap();
        assert!(head.starts_with("CONNECT example.com:443 HTTP/1.1\r\n"));
        assert!(head.contains(&format!("Proxy-Authorization: {AUTH}")));
    }

    #[tokio::test]
    async fn a_refused_login_reaches_chrome_as_502_never_407() {
        let (port, _seen) = fake_http_proxy().await;
        let relay = relay_to(&format!("http://u:wrong@127.0.0.1:{port}")).await;
        let out = send(&relay, b"CONNECT example.com:443 HTTP/1.1\r\n\r\n").await;
        assert!(out.starts_with("HTTP/1.1 502 "), "{out}");
        assert!(!out.contains("407") && !out.contains("Proxy-Authenticate"));
        let out = send(
            &relay,
            b"GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n",
        )
        .await;
        assert!(out.starts_with("HTTP/1.1 502 "), "{out}");
    }

    #[tokio::test]
    async fn plain_http_is_forwarded_with_the_login_and_without_hop_headers() {
        let (port, mut seen) = fake_http_proxy().await;
        let relay = relay_to(&format!("http://u:p@127.0.0.1:{port}")).await;
        let out = send(
            &relay,
            b"GET http://example.com/a HTTP/1.1\r\nHost: example.com\r\nProxy-Connection: keep-alive\r\nProxy-Authorization: Basic Y2xpZW50\r\n\r\n",
        )
        .await;
        assert_eq!(out, "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
        let head = seen.recv().await.unwrap();
        assert_eq!(
            head,
            format!(
                "GET http://example.com/a HTTP/1.1\r\nHost: example.com\r\nProxy-Authorization: {AUTH}\r\nConnection: close\r\n\r\n"
            )
        );
    }

    #[tokio::test]
    async fn requests_a_web_page_could_send_are_refused_without_dialling() {
        let (port, mut seen) = fake_http_proxy().await;
        let relay = relay_to(&format!("http://u:p@127.0.0.1:{port}")).await;
        let out = send(&relay, b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await;
        assert!(out.starts_with("HTTP/1.1 400 "), "{out}");
        let out = send(&relay, b"POST https://example.com/ HTTP/1.1\r\n\r\n").await;
        assert!(out.starts_with("HTTP/1.1 400 "), "{out}");
        assert!(seen.try_recv().is_err());
    }

    #[tokio::test]
    async fn private_upstreams_are_refused_unless_allowed() {
        let err = Relay::start(
            ProxyUpstream::parse("http://127.0.0.1:3128").unwrap(),
            RelayOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ProxyError::AddressNotAllowed(_)));
        let err = Relay::start(
            ProxyUpstream::parse("http://10.0.0.1:3128").unwrap(),
            RelayOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ProxyError::AddressNotAllowed(_)));
    }

    #[tokio::test]
    async fn dropping_the_relay_closes_its_port_and_open_tunnels() {
        let (port, _seen) = fake_http_proxy().await;
        let relay = relay_to(&format!("http://u:p@127.0.0.1:{port}")).await;
        let relay_port = relay.port();
        let mut tunnel = TcpStream::connect(("127.0.0.1", relay_port)).await.unwrap();
        tunnel
            .write_all(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut buf = [0u8; 64];
        let _ = tunnel.read(&mut buf).await.unwrap();
        drop(relay);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(TcpStream::connect(("127.0.0.1", relay_port)).await.is_err());
        let closed = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match tunnel.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        })
        .await;
        assert!(closed.is_ok(), "open tunnel should close with the relay");
    }

    #[tokio::test]
    async fn socks5_upstream_carries_plain_http_in_origin_form() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut greet = [0u8; 4];
            s.read_exact(&mut greet).await.unwrap();
            s.write_all(&[5, 2]).await.unwrap();
            let mut ulen = [0u8; 2];
            s.read_exact(&mut ulen).await.unwrap();
            let mut rest = vec![0u8; usize::from(ulen[1]) + 1];
            s.read_exact(&mut rest).await.unwrap();
            let mut pass = vec![0u8; usize::from(rest[rest.len() - 1])];
            s.read_exact(&mut pass).await.unwrap();
            s.write_all(&[1, 0]).await.unwrap();
            let mut req = [0u8; 5];
            s.read_exact(&mut req).await.unwrap();
            let mut name = vec![0u8; usize::from(req[4]) + 2];
            s.read_exact(&mut name).await.unwrap();
            s.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await.unwrap();
            let head = read_head(&mut s, Vec::new()).await.unwrap().unwrap();
            tx.send(String::from_utf8_lossy(&head.head).to_string())
                .unwrap();
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .await
                .unwrap();
        });
        let relay = relay_to(&format!("socks5://u:p@127.0.0.1:{port}")).await;
        let out = send(
            &relay,
            b"GET http://example.com/x?y HTTP/1.1\r\nHost: example.com\r\n\r\n",
        )
        .await;
        assert!(out.ends_with("ok"), "{out}");
        assert_eq!(
            rx.recv().await.unwrap(),
            "GET /x?y HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n"
        );
    }
}
