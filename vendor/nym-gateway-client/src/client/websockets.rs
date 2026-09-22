#[cfg(not(target_arch = "wasm32"))]
use crate::client::GatewayListeners;
use crate::error::GatewayClientError;

use nym_http_api_client::HickoryDnsResolver;
#[cfg(unix)]
use std::{
    os::fd::{AsRawFd, RawFd},
    sync::Arc,
};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tungstenite::handshake::client::Response;
use url::{Host, Url};

use std::net::SocketAddr;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) async fn connect_async_with_hickory(
    endpoint: &str,
    #[cfg(unix)] connection_fd_callback: Option<Arc<dyn Fn(RawFd) + Send + Sync>>,
) -> Result<(WebSocketStream<MaybeTlsStream<TcpStream>>, Response), GatewayClientError> {
    use tokio::net::TcpSocket;

    let resolver = HickoryDnsResolver::new();
    let uri =
        Url::parse(endpoint).map_err(|_| GatewayClientError::InvalidUrl(endpoint.to_owned()))?;
    let port: u16 = uri.port_or_known_default().unwrap_or(443);

    let host = uri
        .host()
        .ok_or(GatewayClientError::InvalidUrl(endpoint.to_owned()))?;

    // Get address for tcp connection, if a domain is provided use our preferred resolver rather than
    // the default std resolve
    let sock_addrs: Vec<SocketAddr> = match host {
        Host::Ipv4(addr) => vec![SocketAddr::new(addr.into(), port)],
        Host::Ipv6(addr) => vec![SocketAddr::new(addr.into(), port)],
        Host::Domain(domain) => {
            // Do a DNS lookup for the domain using our custom DNS resolver
            resolver
                .resolve_str(domain)
                .await?
                .map(|a| SocketAddr::new(a, port))
                .collect()
        }
    };

    let mut stream = Err(GatewayClientError::NoEndpointForConnection {
        address: endpoint.to_owned(),
    });
    for sock_addr in sock_addrs {
        let socket = if sock_addr.is_ipv4() {
            TcpSocket::new_v4()
        } else {
            TcpSocket::new_v6()
        }
        .map_err(|err| GatewayClientError::NetworkConnectionFailed {
            address: endpoint.to_owned(),
            source: Box::new(tungstenite::Error::from(err)),
        })?;

        #[cfg(unix)]
        if let Some(callback) = connection_fd_callback.as_ref() {
            callback.as_ref()(socket.as_raw_fd());
        }

        match socket.connect(sock_addr).await {
            Ok(s) => {
                stream = Ok(s);
                break;
            }
            Err(err) => {
                stream = Err(GatewayClientError::NetworkConnectionFailed {
                    address: endpoint.to_owned(),
                    source: Box::new(tungstenite::Error::from(err)),
                });
                continue;
            }
        }
    }

    tokio_tungstenite::client_async_tls(endpoint, stream?)
        .await
        .map_err(|error| GatewayClientError::NetworkConnectionFailed {
            address: endpoint.to_owned(),
            source: Box::new(error),
        })
}

// ---- tokumai patch (see vendor/README.md) ----------------------------------------------
// Inside an AWS Nitro enclave there is no network: every connection leaves through an HTTP
// CONNECT proxy on the loopback (forwarded over vsock to the host). With
// TOKUMAI_EGRESS_PROXY=host:port set, the gateway connection goes there: CONNECT to the
// gateway's own host and port (resolved outside, by the proxy), then the WebSocket and its
// TLS over that tunnel, end to end. Unset, nothing changes.
fn egress_proxy() -> Option<String> {
    std::env::var("TOKUMAI_EGRESS_PROXY").ok().filter(|p| !p.trim().is_empty())
}

async fn connect_via_proxy(
    endpoint: &str,
    proxy: &str,
) -> Result<(WebSocketStream<MaybeTlsStream<TcpStream>>, Response), GatewayClientError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let failed = |e: std::io::Error| GatewayClientError::NetworkConnectionFailed {
        address: endpoint.to_owned(),
        source: Box::new(tungstenite::Error::from(e)),
    };
    let uri = Url::parse(endpoint).map_err(|_| GatewayClientError::InvalidUrl(endpoint.to_owned()))?;
    let port: u16 = uri.port_or_known_default().unwrap_or(443);
    let host = match uri.host().ok_or(GatewayClientError::InvalidUrl(endpoint.to_owned()))? {
        Host::Ipv6(a) => format!("[{a}]"),
        h => h.to_string(),
    };
    let mut stream = TcpStream::connect(proxy.trim()).await.map_err(failed)?;
    let request = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n\r\n");
    stream.write_all(request.as_bytes()).await.map_err(failed)?;
    // The proxy's answer, byte by byte up to the blank line, so nothing of the tunnel is read.
    let mut head = Vec::with_capacity(128);
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 4096 || stream.read(&mut byte).await.map_err(failed)? == 0 {
            return Err(failed(std::io::Error::new(std::io::ErrorKind::InvalidData, "the egress proxy did not answer")));
        }
        head.push(byte[0]);
    }
    let status = String::from_utf8_lossy(&head);
    if !status.starts_with("HTTP/1.1 200") && !status.starts_with("HTTP/1.0 200") {
        let line = status.lines().next().unwrap_or("").to_string();
        return Err(failed(std::io::Error::new(std::io::ErrorKind::PermissionDenied, format!("the egress proxy refused: {line}"))));
    }
    tokio_tungstenite::client_async_tls(endpoint, stream)
        .await
        .map_err(|error| GatewayClientError::NetworkConnectionFailed { address: endpoint.to_owned(), source: Box::new(error) })
}
// ---- end of tokumai patch --------------------------------------------------------------

async fn connect_async_inner(
    endpoint: &str,
    use_hickory_dns_resolver: bool,
    #[cfg(unix)] connection_fd_callback: Option<Arc<dyn Fn(RawFd) + Send + Sync>>,
) -> Result<(WebSocketStream<MaybeTlsStream<TcpStream>>, Response), GatewayClientError> {
    if let Some(proxy) = egress_proxy() {
        return connect_via_proxy(endpoint, &proxy).await;
    }
    if use_hickory_dns_resolver {
        connect_async_with_hickory(
            endpoint,
            #[cfg(unix)]
            connection_fd_callback.clone(),
        )
        .await
    } else {
        let (stream, response) = tokio_tungstenite::connect_async(endpoint).await?;
        #[cfg(unix)]
        if let (Some(callback), Some(fd)) = (
            connection_fd_callback.as_ref(),
            crate::socket_state::ws_fd(&stream),
        ) {
            callback.as_ref()(fd);
        }

        Ok((stream, response))
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) async fn connect_async_with_fallback(
    endpoints: &GatewayListeners,
    use_hickory_dns_resolver: bool,
    #[cfg(unix)] connection_fd_callback: Option<Arc<dyn Fn(RawFd) + Send + Sync>>,
) -> Result<(WebSocketStream<MaybeTlsStream<TcpStream>>, Response), GatewayClientError> {
    match connect_async_inner(
        endpoints.primary.as_ref(),
        use_hickory_dns_resolver,
        #[cfg(unix)]
        connection_fd_callback.clone(),
    )
    .await
    {
        Ok(inner) => Ok(inner),
        Err(e) => {
            if let Some(fallback) = &endpoints.fallback {
                tracing::warn!(
                    "Main endpoint failed {} : {e}, trying fallback : {fallback}",
                    endpoints.primary
                );
                connect_async_inner(
                    fallback.as_ref(),
                    use_hickory_dns_resolver,
                    #[cfg(unix)]
                    connection_fd_callback,
                )
                .await
            } else {
                Err(e)
            }
        }
    }
}
