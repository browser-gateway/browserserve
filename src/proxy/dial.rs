//! Opening connections to the upstream proxy and tunnels through it.

use super::address::resolve_allowed;
use super::head::{read_head, response_status};
use super::upstream::{ProxyScheme, ProxyUpstream};
use super::{ProxyError, socks};
use std::io;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

/// A byte stream to the upstream proxy: plain TCP or TLS over TCP.
pub(super) enum UpstreamStream {
    Tcp(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl AsyncRead for UpstreamStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(s) => Pin::new(s).poll_read(cx, buf),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for UpstreamStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Tcp(s) => Pin::new(s).poll_write(cx, buf),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(s) => Pin::new(s).poll_flush(cx),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(s) => Pin::new(s).poll_shutdown(cx),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

/// Connects to one upstream proxy. Every call re-checks nothing: the address
/// list was resolved and checked once when the dialer was built.
pub(super) struct Dialer {
    upstream: ProxyUpstream,
    addrs: Vec<std::net::SocketAddr>,
    tls: Option<TlsConnector>,
    timeout: Duration,
}

impl Dialer {
    /// Resolves and checks the upstream address once.
    pub(super) async fn new(
        upstream: ProxyUpstream,
        allow_private: bool,
        timeout: Duration,
    ) -> Result<Self, ProxyError> {
        let addrs = resolve_allowed(upstream.host(), upstream.port(), allow_private).await?;
        let tls = match upstream.scheme() {
            ProxyScheme::Https => Some(tls_connector()?),
            ProxyScheme::Http | ProxyScheme::Socks5 => None,
        };
        Ok(Self {
            upstream,
            addrs,
            tls,
            timeout,
        })
    }

    pub(super) fn upstream(&self) -> &ProxyUpstream {
        &self.upstream
    }

    /// A fresh connection to the proxy itself (TLS done for `https://`).
    pub(super) async fn open(&self) -> Result<UpstreamStream, ProxyError> {
        tokio::time::timeout(self.timeout, self.open_inner())
            .await
            .map_err(|_| ProxyError::Timeout)?
    }

    async fn open_inner(&self) -> Result<UpstreamStream, ProxyError> {
        let tcp = TcpStream::connect(self.addrs.as_slice())
            .await
            .map_err(|e| ProxyError::Unreachable(e.kind().to_string()))?;
        let _ = tcp.set_nodelay(true);
        let Some(tls) = &self.tls else {
            return Ok(UpstreamStream::Tcp(tcp));
        };
        let name = rustls::pki_types::ServerName::try_from(self.upstream.host().to_string())
            .map_err(|_| ProxyError::Unreachable("invalid TLS server name".into()))?;
        let stream = tls
            .connect(name, tcp)
            .await
            .map_err(|e| ProxyError::Unreachable(format!("TLS: {e}")))?;
        Ok(UpstreamStream::Tls(Box::new(stream)))
    }

    /// Opens a tunnel to `host:port` through the proxy. Returns the stream and
    /// any bytes the proxy sent after accepting the tunnel.
    pub(super) async fn tunnel(
        &self,
        host: &str,
        port: u16,
    ) -> Result<(UpstreamStream, Vec<u8>), ProxyError> {
        tokio::time::timeout(self.timeout, async {
            let mut stream = self.open_inner().await?;
            let rest = match self.upstream.scheme() {
                ProxyScheme::Socks5 => {
                    let login = self
                        .upstream
                        .username()
                        .map(|u| (u, self.upstream.password().unwrap_or("")));
                    socks::connect(&mut stream, host, port, login).await?;
                    Vec::new()
                }
                ProxyScheme::Http | ProxyScheme::Https => {
                    self.http_connect(&mut stream, host, port).await?
                }
            };
            Ok((stream, rest))
        })
        .await
        .map_err(|_| ProxyError::Timeout)?
    }

    async fn http_connect(
        &self,
        stream: &mut UpstreamStream,
        host: &str,
        port: u16,
    ) -> Result<Vec<u8>, ProxyError> {
        let io = |e: io::Error| ProxyError::Unreachable(e.kind().to_string());
        let authority = authority(host, port);
        let mut req = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
        if let Some(auth) = self.upstream.basic_auth() {
            req.push_str("Proxy-Authorization: ");
            req.push_str(&auth);
            req.push_str("\r\n");
        }
        req.push_str("\r\n");
        let req = zeroize::Zeroizing::new(req);
        stream.write_all(req.as_bytes()).await.map_err(io)?;
        let head = read_head(stream, Vec::new())
            .await
            .map_err(io)?
            .ok_or_else(|| ProxyError::Unreachable("proxy closed the connection".into()))?;
        match response_status(&head.head) {
            Some(200..=299) => Ok(head.rest),
            Some(401 | 407) => Err(ProxyError::LoginFailed),
            Some(code) => Err(ProxyError::Refused(format!("status {code}"))),
            None => Err(ProxyError::Unreachable("malformed proxy response".into())),
        }
    }
}

/// `host:port`, with brackets around an IPv6 literal.
pub(super) fn authority(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn tls_connector() -> Result<TlsConnector, ProxyError> {
    static CONFIG: OnceLock<Option<Arc<rustls::ClientConfig>>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots = rustls::RootCertStore::empty();
            let loaded = rustls_native_certs::load_native_certs();
            let (added, _) = roots.add_parsable_certificates(loaded.certs);
            if added == 0 {
                tracing::warn!(
                    "proxy: no system CA certificates found; https proxies cannot be verified"
                );
                return None;
            }
            let config = rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .ok()?
            .with_root_certificates(roots)
            .with_no_client_auth();
            Some(Arc::new(config))
        })
        .clone()
        .map(TlsConnector::from)
        .ok_or_else(|| ProxyError::Unreachable("TLS is not available on this host".into()))
}
