//! SOCKS5 client handshake with optional username/password login
//! (RFC 1928 and RFC 1929). The target host name is sent as is, so the proxy
//! resolves it.

use super::ProxyError;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const VERSION: u8 = 0x05;
const NO_AUTH: u8 = 0x00;
const USER_PASS: u8 = 0x02;
const NO_ACCEPTABLE: u8 = 0xff;

/// Asks the SOCKS5 proxy on `stream` to connect to `host:port`.
///
/// # Errors
///
/// [`ProxyError::LoginFailed`] when the login is refused,
/// [`ProxyError::Refused`] when the proxy will not open the connection,
/// [`ProxyError::Unreachable`] on protocol or I/O failure.
pub(super) async fn connect<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    host: &str,
    port: u16,
    login: Option<(&str, &str)>,
) -> Result<(), ProxyError> {
    let io = |e: std::io::Error| ProxyError::Unreachable(format!("socks: {e}"));
    let greeting: &[u8] = if login.is_some() {
        &[VERSION, 2, NO_AUTH, USER_PASS]
    } else {
        &[VERSION, 1, NO_AUTH]
    };
    stream.write_all(greeting).await.map_err(io)?;
    let mut choice = [0u8; 2];
    stream.read_exact(&mut choice).await.map_err(io)?;
    if choice[0] != VERSION {
        return Err(ProxyError::Unreachable("socks: not a SOCKS5 proxy".into()));
    }
    match (choice[1], login) {
        (NO_AUTH, _) => {}
        (USER_PASS, Some((user, pass))) => {
            let user = u8::try_from(user.len()).map_err(|_| ProxyError::LoginFailed)?;
            let pass_len = u8::try_from(pass.len()).map_err(|_| ProxyError::LoginFailed)?;
            let (u, p) = login
                .map(|(u, p)| (u.as_bytes(), p.as_bytes()))
                .unwrap_or_default();
            let mut msg = Vec::with_capacity(3 + u.len() + p.len());
            msg.extend_from_slice(&[0x01, user]);
            msg.extend_from_slice(u);
            msg.push(pass_len);
            msg.extend_from_slice(p);
            stream.write_all(&msg).await.map_err(io)?;
            let mut status = [0u8; 2];
            stream.read_exact(&mut status).await.map_err(io)?;
            if status[1] != 0 {
                return Err(ProxyError::LoginFailed);
            }
        }
        (USER_PASS | NO_ACCEPTABLE, None) | (NO_ACCEPTABLE, Some(_)) => {
            return Err(ProxyError::LoginFailed);
        }
        _ => return Err(ProxyError::Unreachable("socks: unsupported method".into())),
    }

    let mut req = vec![VERSION, 0x01, 0x00];
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => {
            req.push(0x01);
            req.extend_from_slice(&v4.octets());
        }
        Ok(std::net::IpAddr::V6(v6)) => {
            req.push(0x04);
            req.extend_from_slice(&v6.octets());
        }
        Err(_) => {
            let len = u8::try_from(host.len())
                .map_err(|_| ProxyError::Refused("target host name too long".into()))?;
            req.extend_from_slice(&[0x03, len]);
            req.extend_from_slice(host.as_bytes());
        }
    }
    req.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&req).await.map_err(io)?;

    let mut reply = [0u8; 4];
    stream.read_exact(&mut reply).await.map_err(io)?;
    if reply[0] != VERSION {
        return Err(ProxyError::Unreachable("socks: bad reply".into()));
    }
    if reply[1] != 0 {
        return Err(ProxyError::Refused(reply_reason(reply[1]).into()));
    }
    let skip = match reply[3] {
        0x01 => 4 + 2,
        0x04 => 16 + 2,
        0x03 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await.map_err(io)?;
            usize::from(len[0]) + 2
        }
        _ => return Err(ProxyError::Unreachable("socks: bad address type".into())),
    };
    let mut bound = vec![0u8; skip];
    stream.read_exact(&mut bound).await.map_err(io)?;
    Ok(())
}

fn reply_reason(code: u8) -> &'static str {
    match code {
        0x02 => "not allowed by the proxy's rules",
        0x03 => "network unreachable",
        0x04 => "host unreachable",
        0x05 => "connection refused",
        0x06 => "timed out",
        _ => "proxy failure",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    async fn fake_server(
        mut s: tokio::io::DuplexStream,
        want_login: Option<(&'static str, &'static str)>,
        reply_code: u8,
    ) -> Vec<u8> {
        let mut head = [0u8; 2];
        s.read_exact(&mut head).await.unwrap();
        let mut methods = vec![0u8; head[1] as usize];
        s.read_exact(&mut methods).await.unwrap();
        if let Some((user, pass)) = want_login {
            s.write_all(&[5, USER_PASS]).await.unwrap();
            let mut v = [0u8; 2];
            s.read_exact(&mut v).await.unwrap();
            let mut u = vec![0u8; v[1] as usize];
            s.read_exact(&mut u).await.unwrap();
            let mut pl = [0u8; 1];
            s.read_exact(&mut pl).await.unwrap();
            let mut p = vec![0u8; pl[0] as usize];
            s.read_exact(&mut p).await.unwrap();
            let ok = u == user.as_bytes() && p == pass.as_bytes();
            s.write_all(&[1, u8::from(!ok)]).await.unwrap();
            if !ok {
                return Vec::new();
            }
        } else {
            s.write_all(&[5, NO_AUTH]).await.unwrap();
        }
        let mut req = [0u8; 5];
        s.read_exact(&mut req).await.unwrap();
        let mut name = vec![0u8; req[4] as usize + 2];
        s.read_exact(&mut name).await.unwrap();
        s.write_all(&[5, reply_code, 0, 1, 1, 2, 3, 4, 0, 80])
            .await
            .unwrap();
        name
    }

    #[tokio::test]
    async fn logs_in_and_sends_the_host_name() {
        let (mut client, server) = duplex(1024);
        let srv = tokio::spawn(fake_server(server, Some(("u", "p")), 0));
        connect(&mut client, "example.com", 443, Some(("u", "p")))
            .await
            .unwrap();
        let name = srv.await.unwrap();
        assert_eq!(&name[..11], b"example.com");
        assert_eq!(&name[11..], &443u16.to_be_bytes());
    }

    #[tokio::test]
    async fn wrong_login_and_refusal_are_reported() {
        let (mut client, server) = duplex(1024);
        tokio::spawn(fake_server(server, Some(("u", "p")), 0));
        assert!(matches!(
            connect(&mut client, "example.com", 443, Some(("u", "bad"))).await,
            Err(ProxyError::LoginFailed)
        ));

        let (mut client, server) = duplex(1024);
        tokio::spawn(fake_server(server, None, 0x05));
        assert!(matches!(
            connect(&mut client, "example.com", 443, None).await,
            Err(ProxyError::Refused(_))
        ));
    }
}
