//! Tunnel - TCP <-> QUIC forwarding.

use anyhow::{Context, Result};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tracing::{debug, error, info};

const LOCALHOST: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);

/// Bind the local listener for a forwarded port. Kept separate from the
/// accept loop so a failed bind (port already in use) surfaces to the
/// caller before any binding is recorded.
pub async fn bind_listener(port: u16) -> Result<TcpListener> {
    let addr = SocketAddr::from((LOCALHOST, port));
    TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {}", addr))
}

/// Accept loop: forward each connection on `listener` to the peer's port
pub async fn serve_listener<P>(listener: TcpListener, port: u16, peer: &P) -> Result<()>
where
    P: PeerConnection + Send + Sync + 'static,
{
    info!("listening on 127.0.0.1:{}", port);

    loop {
        let (stream, client_addr) = listener.accept().await?;
        debug!("accepted connection from {} on port {}", client_addr, port);

        // Open connection to peer for this port
        match peer.open_tunnel(port).await {
            Ok((send, recv)) => {
                tokio::spawn(async move {
                    if let Err(e) = forward_bidirectional(stream, send, recv).await {
                        error!("tunnel error: {}", e);
                    }
                });
            }
            Err(e) => {
                error!("failed to open tunnel to peer for port {}: {}", port, e);
            }
        }
    }
}

/// Forward to any `host:port` this machine can reach.
///
/// The name is resolved here, on the serving machine, which is what makes an
/// internal-only name work at all -- and the connection originates here, so the
/// upstream sees this machine's address rather than the peer's.
pub async fn forward_to_tcp(
    upstream: &str,
    send: iroh::endpoint::SendStream,
    recv: iroh::endpoint::RecvStream,
) -> Result<()> {
    let stream = TcpStream::connect(upstream)
        .await
        .with_context(|| format!("failed to connect to {}", upstream))?;

    forward_bidirectional(stream, send, recv).await
}

/// Forward to a Unix socket on this machine.
pub async fn forward_to_unix(
    path: &Path,
    send: iroh::endpoint::SendStream,
    recv: iroh::endpoint::RecvStream,
) -> Result<()> {
    let stream = UnixStream::connect(path)
        .await
        .with_context(|| format!("failed to connect to {}", path.display()))?;

    forward_bidirectional(stream, send, recv).await
}

/// Bidirectional forwarding between a local stream and the QUIC streams.
///
/// Generic over the local side so a TCP socket and a Unix socket take the same
/// path; `tokio::io::split` is what makes that possible, since the concrete
/// `into_split` methods have no common trait.
async fn forward_bidirectional<S>(
    local: S,
    mut quic_send: iroh::endpoint::SendStream,
    mut quic_recv: iroh::endpoint::RecvStream,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite,
{
    let (mut local_read, mut local_write) = tokio::io::split(local);

    let local_to_quic = async {
        let result = tokio::io::copy(&mut local_read, &mut quic_send).await;
        let _ = quic_send.finish();
        result
    };

    let quic_to_local = async { tokio::io::copy(&mut quic_recv, &mut local_write).await };

    tokio::select! {
        r = local_to_quic => { debug!("local->quic ended: {:?}", r); }
        r = quic_to_local => { debug!("quic->local ended: {:?}", r); }
    }

    Ok(())
}

/// Trait for opening tunnels to a peer
pub trait PeerConnection: Send + Sync {
    fn open_tunnel(
        &self,
        port: u16,
    ) -> impl std::future::Future<
        Output = Result<(iroh::endpoint::SendStream, iroh::endpoint::RecvStream)>,
    > + Send;
}
