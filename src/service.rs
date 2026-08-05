//! Services -- what an exposed port is backed by.
//!
//! By default an exposed port forwards to `--host:<port>` on the serving
//! machine. That is one fixed address for every port, which is enough when the
//! thing you want is listening locally, and not enough when it lives behind the
//! serving machine: an internal site that resolves only there, a database on
//! another internal host, a daemon that only speaks over a Unix socket.
//!
//! A service declares what a port is backed by instead. Three kinds:
//!
//!   - `http`  -- a reverse proxy, routing by Host and path to internal upstreams
//!   - `tcp`   -- a plain forward to any `host:port` the serving machine can reach
//!   - `unix`  -- a plain forward to a Unix socket on the serving machine
//!
//! In all three the connection is made by the serving machine: names resolve
//! with its resolver, and the upstream sees its address, not the peer's. Ports
//! with no service declared keep the `--host:<port>` default.
//!
//! Nothing about access changes. A service port is exposed and granted like any
//! other, so grants, enrollment, auto-binding and reconnection are untouched.
//!
//! The HTTP kind uses pingora as a library, not a fork. Its transport
//! abstraction is `Box<dyn IO>` and `IO` is blanket-implemented for anything
//! satisfying its supertraits, so an iroh stream only has to implement those to
//! be accepted everywhere a TCP socket would be.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};
use async_trait::async_trait;
use iroh::endpoint::{RecvStream, SendStream};
use iroh::EndpointId;
use pingora_core::apps::ServerApp;
use pingora_core::protocols::l4::socket::SocketAddr as PingoraSockAddr;
use pingora_core::protocols::raw_connect::ProxyDigest;
use pingora_core::protocols::{
    GetProxyDigest, GetSocketDigest, GetTimingDigest, Peek, Shutdown, SocketDigest, Ssl,
    TimingDigest, UniqueID, UniqueIDType,
};
use pingora_core::server::configuration::ServerConf;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_proxy::{HttpProxy, ProxyHttp, Session};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::tunnel;

// ============================================================================
// Configuration
// ============================================================================

/// What a port is backed by. Travels over the control socket, so `expose` can
/// declare a backend at runtime exactly as the startup file does at boot.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum BackendSpec {
    /// Reverse proxy. Routes are tried in order; the first match wins.
    Http {
        #[serde(default, rename = "route")]
        routes: Vec<Route>,
    },
    /// Plain forward to `upstream`, resolved by the serving machine.
    Tcp { upstream: String },
    /// Plain forward to a Unix socket on the serving machine.
    Unix { path: PathBuf },
}

impl BackendSpec {
    pub fn validate(&self, port: u16) -> Result<()> {
        match self {
            BackendSpec::Http { routes } => {
                if routes.is_empty() {
                    bail!("http service on port {port} has no routes");
                }
                for route in routes {
                    route
                        .validate()
                        .with_context(|| format!("http service on port {port}"))?;
                }
            }
            BackendSpec::Tcp { upstream } => {
                if upstream.is_empty() {
                    bail!("tcp service on port {port} has an empty upstream");
                }
            }
            BackendSpec::Unix { path } => {
                if path.as_os_str().is_empty() {
                    bail!("unix service on port {port} has an empty path");
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct ServiceFile {
    #[serde(default, rename = "service")]
    services: Vec<ServiceEntry>,
}

/// One entry of the startup file: a port plus the backend it declares.
#[derive(Debug, Deserialize)]
struct ServiceEntry {
    port: u16,
    #[serde(flatten)]
    backend: BackendSpec,
}

/// A file holding just a route table, for `expose --routes`.
#[derive(Debug, Deserialize)]
struct RoutesFile {
    #[serde(default, rename = "route")]
    routes: Vec<Route>,
}

fn load_toml<T: DeserializeOwned>(path: &Path, what: &str) -> Result<T> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {what} {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("failed to parse {what} {}", path.display()))
}

/// Read a `[[route]]` table for `expose --routes`. Parsed by the CLI so a bad
/// table is reported where it was typed, not swallowed by the daemon.
pub fn load_routes(path: &Path) -> Result<Vec<Route>> {
    let file: RoutesFile = load_toml(path, "route table")?;
    if file.routes.is_empty() {
        bail!("route table {} has no [[route]] entries", path.display());
    }
    Ok(file.routes)
}

/// Read the startup file: the same declarations `expose` makes, applied at boot.
/// Backends are validated where they are registered, not here, so both paths
/// reject the same things in the same words.
pub fn load_service_file(path: &Path) -> Result<Vec<(u16, BackendSpec)>> {
    let file: ServiceFile = load_toml(path, "service config")?;
    if file.services.is_empty() {
        bail!(
            "service config {} has no [[service]] entries",
            path.display()
        );
    }
    Ok(file
        .services
        .into_iter()
        .map(|s| (s.port, s.backend))
        .collect())
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Route {
    /// Match the request's Host header (hostname only, port ignored).
    /// Absent matches any host.
    pub host: Option<String>,
    /// Match a path prefix. Absent matches any path.
    pub prefix: Option<String>,
    /// Where to send it, e.g. "gitlab.internal:80". Resolved by the serving
    /// machine's resolver at request time, so internal-only names work and DNS
    /// changes take effect without a restart. Mutually exclusive with `unix`.
    pub upstream: Option<String>,
    /// Send it to an HTTP server listening on a Unix socket instead.
    /// Mutually exclusive with `upstream`.
    pub unix: Option<PathBuf>,
    /// Speak TLS to the upstream.
    #[serde(default)]
    pub tls: bool,
    /// SNI to present upstream. Defaults to the upstream hostname.
    pub sni: Option<String>,
    /// Strip `prefix` from the path before forwarding.
    #[serde(default)]
    pub strip_prefix: bool,
    /// Host header to send upstream. Defaults to the upstream hostname.
    pub host_header: Option<String>,
}

impl Route {
    fn validate(&self) -> Result<()> {
        match (&self.upstream, &self.unix) {
            (Some(u), None) if u.is_empty() => bail!("route has an empty upstream"),
            (Some(_), None) | (None, Some(_)) => Ok(()),
            (None, None) => bail!("route needs `upstream` or `unix`"),
            (Some(_), Some(_)) => bail!("route sets both `upstream` and `unix`"),
        }
    }

    /// Hostname the upstream is known by: the host part of `upstream`, or for a
    /// Unix socket whatever `sni` says, falling back to "localhost" so the
    /// Host header and SNI are at least well-formed.
    fn upstream_host(&self) -> &str {
        match &self.upstream {
            Some(upstream) => match upstream.rsplit_once(':') {
                // Guard against IPv6 literals like "[::1]" that have no port.
                Some((head, tail)) if !tail.contains(']') => head,
                _ => upstream.as_str(),
            },
            None => self.sni.as_deref().unwrap_or("localhost"),
        }
    }

    fn matches(&self, host: Option<&str>, path: &str) -> bool {
        if let Some(want) = &self.host {
            let got = host.map(|h| h.split(':').next().unwrap_or(h));
            if got != Some(want.as_str()) {
                return false;
            }
        }
        if let Some(prefix) = &self.prefix {
            if !path.starts_with(prefix.as_str()) {
                return false;
            }
        }
        true
    }
}

// ============================================================================
// Registry
// ============================================================================

/// Cheap to clone so `serve` can take a handle and drop the lock before it
/// starts awaiting -- a proxied HTTP connection lives as long as the peer keeps
/// it, which is far too long to hold a registry lock for.
#[derive(Clone)]
enum Backend {
    Http {
        proxy: Arc<HttpProxy<Gateway>>,
        routes: usize,
    },
    Tcp(String),
    Unix(PathBuf),
}

impl Backend {
    fn describe(&self) -> String {
        match self {
            Backend::Http { routes, .. } => {
                format!(
                    "http ({routes} route{})",
                    if *routes == 1 { "" } else { "s" }
                )
            }
            Backend::Tcp(upstream) => format!("tcp {upstream}"),
            Backend::Unix(path) => format!("unix {}", path.display()),
        }
    }
}

/// What each exposed port is backed by, keyed by the port peers ask for.
///
/// Mutable at runtime, because everything else about an exposed port is: a port
/// is granted and revoked live over the control socket, and a backend that
/// needed a daemon restart would be the one static thing in a dynamic system.
pub struct Services {
    backends: std::sync::RwLock<HashMap<u16, Backend>>,
    /// Where a port with no declared backend forwards to. Modelled as a
    /// fallback backend rather than a branch at the call site, so every port is
    /// served by exactly one mechanism.
    host: IpAddr,
    conf: Arc<ServerConf>,
    /// pingora wants a shutdown channel; holding the sender keeps the receiver
    /// from reporting shutdown for the daemon's lifetime.
    _shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
}

impl Services {
    pub fn new(host: IpAddr) -> Self {
        let (tx, rx) = watch::channel(false);
        Self {
            backends: std::sync::RwLock::new(HashMap::new()),
            host,
            conf: Arc::new(ServerConf::default()),
            _shutdown_tx: tx,
            shutdown_rx: rx,
        }
    }

    fn build(&self, port: u16, spec: BackendSpec) -> Result<Backend> {
        spec.validate(port)?;
        Ok(match spec {
            BackendSpec::Http { routes } => Backend::Http {
                routes: routes.len(),
                proxy: Arc::new(pingora_proxy::http_proxy(
                    &self.conf,
                    Gateway {
                        port,
                        routes,
                        resolved: Mutex::new(HashMap::new()),
                    },
                )),
            },
            BackendSpec::Tcp { upstream } => Backend::Tcp(upstream),
            BackendSpec::Unix { path } => Backend::Unix(path),
        })
    }

    /// Declare what `port` is backed by, replacing any previous declaration.
    /// Re-exposing a port with a new backend is a redirect, not an error.
    pub fn register(&self, port: u16, spec: BackendSpec) -> Result<()> {
        let backend = self.build(port, spec)?;
        info!("service on port {} ({})", port, backend.describe());
        self.backends.write().unwrap().insert(port, backend);
        Ok(())
    }

    /// Same, but refuse to overwrite. Used when applying the startup file, where
    /// two entries for one port is a mistake rather than an intent to replace.
    /// Startup is single threaded, so the check need not share the write lock.
    pub fn register_new(&self, port: u16, spec: BackendSpec) -> Result<()> {
        if self.contains(port) {
            bail!("two services both claim port {port}");
        }
        self.register(port, spec)
    }

    pub fn remove(&self, port: u16) -> bool {
        self.backends.write().unwrap().remove(&port).is_some()
    }

    /// Ports backed by a service. These are exposed and granted like any other
    /// port, but never bound locally on this side.
    pub fn ports(&self) -> Vec<u16> {
        let mut ports: Vec<u16> = self.backends.read().unwrap().keys().copied().collect();
        ports.sort_unstable();
        ports
    }

    pub fn contains(&self, port: u16) -> bool {
        self.backends.read().unwrap().contains_key(&port)
    }

    /// What serves `port`: its declared backend, or the default forward.
    fn backend(&self, port: u16) -> Backend {
        self.backends
            .read()
            .unwrap()
            .get(&port)
            .cloned()
            .unwrap_or_else(|| Backend::Tcp(SocketAddr::new(self.host, port).to_string()))
    }

    /// One-line description of what `port` is backed by, for `list`. An
    /// undeclared port says so rather than reporting its equivalent forward,
    /// because "nothing was declared here" is what a reader wants to know.
    pub fn describe(&self, port: u16) -> String {
        match self.backends.read().unwrap().get(&port) {
            Some(backend) => backend.describe(),
            None => format!("default {}:{}", self.host, port),
        }
    }

    /// Serve one tunnel stream from its backend. Returns when the peer is done
    /// with the stream.
    pub async fn serve(
        &self,
        port: u16,
        peer: EndpointId,
        send: SendStream,
        recv: RecvStream,
    ) -> Result<()> {
        // Take a handle and let the lock go: the match arms below await for the
        // lifetime of the tunnelled connection.
        match self.backend(port) {
            Backend::Http { proxy, .. } => {
                // process_new hands the stream back when the connection is
                // reusable, so keep feeding it to honour HTTP keep-alive.
                let mut stream: pingora_core::protocols::Stream =
                    Box::new(IrohStream::new(send, recv, peer));
                while let Some(reused) = proxy.process_new(stream, &self.shutdown_rx).await {
                    stream = reused;
                }
                Ok(())
            }
            Backend::Tcp(upstream) => tunnel::forward_to_tcp(&upstream, send, recv).await,
            Backend::Unix(path) => tunnel::forward_to_unix(&path, send, recv).await,
        }
    }
}

// ============================================================================
// The HTTP kind
// ============================================================================

/// Which route `request_filter` picked. An index rather than the route itself:
/// `Gateway` outlives every request it serves, so the later hooks can just look
/// it up again instead of each request carrying its own deep copy.
pub struct Ctx {
    route: Option<usize>,
}

/// How long a resolved upstream address is reused.
///
/// Short enough that a DNS change still takes effect without a restart, long
/// enough that a busy port is not resolving on every request. The reason this
/// matters is not the lookup cost: pingora keys its upstream connection pool on
/// the resolved address, so re-resolving a name whose records rotate hands it a
/// different key each time and defeats keep-alive entirely.
const DNS_TTL: Duration = Duration::from_secs(5);

pub struct Gateway {
    port: u16,
    routes: Vec<Route>,
    resolved: Mutex<HashMap<String, (SocketAddr, Instant)>>,
}

impl Gateway {
    fn pick(&self, host: Option<&str>, path: &str) -> Option<usize> {
        self.routes.iter().position(|r| r.matches(host, path))
    }

    fn route(&self, ctx: &Ctx) -> Option<&Route> {
        ctx.route.map(|i| &self.routes[i])
    }

    /// Resolve an upstream with this machine's resolver, reusing a recent
    /// answer. The lock is never held across the lookup.
    async fn resolve(&self, upstream: &str) -> pingora_core::Result<SocketAddr> {
        if let Some(addr) = self.cached(upstream) {
            return Ok(addr);
        }

        let addr = tokio::net::lookup_host(upstream)
            .await
            .map_err(|e| {
                pingora_core::Error::explain(
                    pingora_core::ErrorType::ConnectProxyFailure,
                    format!("cannot resolve upstream {upstream}: {e}"),
                )
            })?
            .next()
            .ok_or_else(|| {
                pingora_core::Error::explain(
                    pingora_core::ErrorType::ConnectProxyFailure,
                    format!("upstream {upstream} resolved to no addresses"),
                )
            })?;

        self.resolved
            .lock()
            .unwrap()
            .insert(upstream.to_string(), (addr, Instant::now()));
        Ok(addr)
    }

    fn cached(&self, upstream: &str) -> Option<SocketAddr> {
        self.resolved
            .lock()
            .unwrap()
            .get(upstream)
            .filter(|(_, at)| at.elapsed() < DNS_TTL)
            .map(|(addr, _)| *addr)
    }
}

#[async_trait]
impl ProxyHttp for Gateway {
    type CTX = Ctx;

    fn new_ctx(&self) -> Ctx {
        Ctx { route: None }
    }

    /// Route selection happens here rather than in `upstream_peer` so that an
    /// unmatched request becomes a clean 404 instead of a proxy error.
    async fn request_filter(
        &self,
        session: &mut Session,
        ctx: &mut Ctx,
    ) -> pingora_core::Result<bool> {
        let req = session.req_header();
        let host = req
            .headers
            .get(http::header::HOST)
            .and_then(|v| v.to_str().ok());

        match self.pick(host, req.uri.path()) {
            Some(index) => {
                debug!("service {}: {} {}", self.port, req.method, req.uri.path());
                ctx.route = Some(index);
                Ok(false)
            }
            None => {
                warn!(
                    "service {}: no route for {:?} {}",
                    self.port,
                    host,
                    req.uri.path()
                );
                session.respond_error(404).await?;
                Ok(true)
            }
        }
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Ctx,
    ) -> pingora_core::Result<Box<HttpPeer>> {
        // request_filter always populates this before we get here.
        let route = self.route(ctx).expect("route chosen in request_filter");
        let sni = route
            .sni
            .clone()
            .unwrap_or_else(|| route.upstream_host().to_string());

        if let Some(path) = &route.unix {
            let path = path.to_str().ok_or_else(|| {
                pingora_core::Error::explain(
                    pingora_core::ErrorType::SocketError,
                    format!("upstream socket path is not utf-8: {}", path.display()),
                )
            })?;
            return Ok(Box::new(HttpPeer::new_uds(path, route.tls, sni)?));
        }

        let upstream = route
            .upstream
            .as_deref()
            .expect("validate() guarantees upstream or unix");

        // Resolved here rather than at config load: the whole point is that the
        // name resolves on this machine, and it may resolve differently over
        // time. HttpPeer::new panics on an unresolvable name, so never hand it
        // one.
        let addr = self.resolve(upstream).await?;

        Ok(Box::new(HttpPeer::new(addr, route.tls, sni)))
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream: &mut pingora_http::RequestHeader,
        ctx: &mut Ctx,
    ) -> pingora_core::Result<()> {
        let Some(route) = self.route(ctx) else {
            return Ok(());
        };

        if route.strip_prefix {
            if let Some(prefix) = &route.prefix {
                let path = upstream.uri.path();
                let rest = path.strip_prefix(prefix.as_str()).unwrap_or(path);
                // Keep the path absolute: stripping "/gitlab" from "/gitlab" or
                // from "/gitlabfoo" must not produce a relative or empty target.
                let rest = if rest.starts_with('/') {
                    rest.to_string()
                } else {
                    format!("/{rest}")
                };
                let rebuilt = match upstream.uri.query() {
                    Some(q) => format!("{rest}?{q}"),
                    None => rest,
                };
                match rebuilt.parse() {
                    Ok(uri) => upstream.set_uri(uri),
                    Err(e) => warn!("service {}: cannot rewrite path: {e}", self.port),
                }
            }
        }

        // The client dialed 127.0.0.1:<port> through the tunnel, so its Host
        // header names the tunnel, not the site. Upstreams that vhost on Host
        // need the real name.
        let host = route
            .host_header
            .clone()
            .unwrap_or_else(|| route.upstream_host().to_string());
        upstream.insert_header(http::header::HOST, &host)?;

        Ok(())
    }
}

// ============================================================================
// iroh stream -> pingora IO
// ============================================================================

static NEXT_ID: AtomicI32 = AtomicI32::new(1);

/// Map an `EndpointId` to a stable synthetic address in the CGNAT range
/// 100.64.0.0/10.
///
/// pingora's logging and any address-based policy want a `SocketAddr`, and a
/// tunnelled stream has none. Handing every peer 127.0.0.1 would merge them into
/// one indistinguishable client; hashing the peer's public key instead gives
/// each one a stable, unique, non-routable address that cannot collide with a
/// real internal network.
fn synthetic_addr(id: &EndpointId) -> PingoraSockAddr {
    let bytes = id.as_bytes();
    let mut h: u32 = 0;
    for chunk in bytes.chunks(4) {
        let mut word = [0u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        h ^= u32::from_be_bytes(word);
        h = h.rotate_left(7);
    }
    // 100.64.0.0/10 leaves 22 host bits.
    let ip = Ipv4Addr::from(0x6440_0000u32 | (h & 0x003F_FFFF));
    let port = 1024 + (h >> 22) as u16;
    PingoraSockAddr::Inet(std::net::SocketAddr::V4(SocketAddrV4::new(ip, port)))
}

#[derive(Debug)]
struct IrohStream {
    send: SendStream,
    recv: RecvStream,
    id: UniqueIDType,
    digest: Arc<SocketDigest>,
}

impl IrohStream {
    fn new(send: SendStream, recv: RecvStream, peer: EndpointId) -> Self {
        // Seed both cells so pingora never queries the (absent) fd. The -1 is
        // only reachable through tcp_info(), which degrades to None.
        let digest = SocketDigest::from_raw_fd(-1);
        let _ = digest.peer_addr.set(Some(synthetic_addr(&peer)));
        let _ = digest
            .local_addr
            .set(Some(PingoraSockAddr::Inet(std::net::SocketAddr::V4(
                SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
            ))));
        let _ = digest.original_dst.set(None);

        Self {
            send,
            recv,
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            digest: Arc::new(digest),
        }
    }
}

impl AsyncRead for IrohStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.recv).poll_read(cx, buf)
    }
}

// SendStream has an inherent poll_write returning WriteError, so every call here
// is fully qualified to reach the tokio trait rather than the inherent method.
impl AsyncWrite for IrohStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        AsyncWrite::poll_write(Pin::new(&mut self.send), cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_flush(Pin::new(&mut self.send), cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_shutdown(Pin::new(&mut self.send), cx)
    }
}

#[async_trait]
impl Shutdown for IrohStream {
    async fn shutdown(&mut self) {
        // finish() is a clean end-of-stream; a reset would look like an error to
        // the client even on an ordinary keep-alive teardown.
        let _ = self.send.finish();
    }
}

impl UniqueID for IrohStream {
    fn id(&self) -> UniqueIDType {
        self.id
    }
}

/// No TLS inside the tunnel: iroh already ran QUIC with mutual public-key
/// authentication one layer down.
impl Ssl for IrohStream {}

impl GetTimingDigest for IrohStream {
    fn get_timing_digest(&self) -> Vec<Option<TimingDigest>> {
        vec![]
    }
}

impl GetProxyDigest for IrohStream {
    fn get_proxy_digest(&self) -> Option<Arc<ProxyDigest>> {
        None
    }
}

impl GetSocketDigest for IrohStream {
    fn get_socket_digest(&self) -> Option<Arc<SocketDigest>> {
        Some(self.digest.clone())
    }
    fn set_socket_digest(&mut self, socket_digest: SocketDigest) {
        self.digest = Arc::new(socket_digest);
    }
}

/// A QUIC stream cannot un-read bytes, so peeking is unsupported. The default
/// `false` makes pingora skip the h2c preface sniff and stay on HTTP/1.1.
#[async_trait]
impl Peek for IrohStream {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write `text` to a temp file and read it back through the real loader, so
    /// the tests exercise `load_service_file` rather than a copy of its body.
    fn parse(text: &str) -> Result<Vec<(u16, BackendSpec)>> {
        let dir = std::env::temp_dir().join(format!(
            "iroh-gate-parse-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("services.toml");
        std::fs::write(&path, text).unwrap();
        let out = load_service_file(&path);
        std::fs::remove_dir_all(&dir).unwrap();
        out
    }

    /// Registering is where a spec is validated, on both the startup and the
    /// `expose` path, so validation tests go through it.
    fn register_all(entries: Result<Vec<(u16, BackendSpec)>>) -> Result<Services> {
        let services = Services::new(Ipv4Addr::LOCALHOST.into());
        for (port, spec) in entries? {
            services.register_new(port, spec)?;
        }
        Ok(services)
    }

    fn route(host: Option<&str>, prefix: Option<&str>, upstream: &str) -> Route {
        Route {
            host: host.map(str::to_string),
            prefix: prefix.map(str::to_string),
            upstream: Some(upstream.to_string()),
            unix: None,
            tls: false,
            sni: None,
            strip_prefix: false,
            host_header: None,
        }
    }

    fn kind_of(spec: &BackendSpec) -> &'static str {
        match spec {
            BackendSpec::Http { .. } => "http",
            BackendSpec::Tcp { .. } => "tcp",
            BackendSpec::Unix { .. } => "unix",
        }
    }

    #[test]
    fn prefix_route_matches_by_path_only() {
        let r = route(None, Some("/gitlab"), "gitlab.internal:80");
        assert!(r.matches(None, "/gitlab/x"));
        assert!(r.matches(Some("anything:8080"), "/gitlab"));
        assert!(!r.matches(None, "/wiki"));
    }

    #[test]
    fn host_route_ignores_port_in_header() {
        let r = route(Some("wiki.local"), None, "wiki.internal:80");
        assert!(r.matches(Some("wiki.local"), "/"));
        assert!(r.matches(Some("wiki.local:8080"), "/"));
        assert!(!r.matches(Some("other.local"), "/"));
        assert!(!r.matches(None, "/"));
    }

    #[test]
    fn upstream_host_strips_port_but_keeps_ipv6_literal() {
        assert_eq!(
            route(None, None, "gitlab.internal:80").upstream_host(),
            "gitlab.internal"
        );
        assert_eq!(
            route(None, None, "gitlab.internal").upstream_host(),
            "gitlab.internal"
        );
        assert_eq!(route(None, None, "[::1]").upstream_host(), "[::1]");
        assert_eq!(route(None, None, "[::1]:80").upstream_host(), "[::1]");
    }

    #[test]
    fn unix_route_names_itself_by_sni_then_localhost() {
        let mut r = route(None, None, "");
        r.upstream = None;
        r.unix = Some(PathBuf::from("/run/app.sock"));
        assert_eq!(r.upstream_host(), "localhost");
        r.sni = Some("app.internal".into());
        assert_eq!(r.upstream_host(), "app.internal");
    }

    #[test]
    fn parses_all_three_kinds() {
        let services = parse(
            r#"
            [[service]]
            kind = "http"
            port = 8080
            [[service.route]]
            prefix = "/gitlab"
            upstream = "gitlab.internal:80"

            [[service]]
            kind = "tcp"
            port = 5432
            upstream = "db.internal:5432"

            [[service]]
            kind = "unix"
            port = 6000
            path = "/var/run/docker.sock"
            "#,
        )
        .unwrap();

        assert_eq!(services.len(), 3);
        assert_eq!(
            services.iter().map(|(_, s)| kind_of(s)).collect::<Vec<_>>(),
            ["http", "tcp", "unix"]
        );
        assert_eq!(
            services.iter().map(|(p, _)| *p).collect::<Vec<_>>(),
            [8080, 5432, 6000]
        );
    }

    #[test]
    fn route_needs_exactly_one_upstream_kind() {
        let neither = register_all(parse(
            r#"
            [[service]]
            kind = "http"
            port = 80
            [[service.route]]
            prefix = "/"
            "#,
        ));
        assert!(
            neither.is_err(),
            "a route with no upstream must be rejected"
        );

        let both = register_all(parse(
            r#"
            [[service]]
            kind = "http"
            port = 80
            [[service.route]]
            prefix = "/"
            upstream = "a.internal:80"
            unix = "/run/a.sock"
            "#,
        ));
        assert!(both.is_err(), "a route with two upstreams must be rejected");
    }

    #[test]
    fn http_service_needs_a_route() {
        let empty = register_all(parse("[[service]]\nkind = \"http\"\nport = 80\n"));
        assert!(empty.is_err());
    }

    #[test]
    fn a_file_with_no_services_is_rejected() {
        assert!(parse("").is_err());
    }

    #[test]
    fn duplicate_ports_are_rejected_at_startup_but_replaced_at_runtime() {
        let services = Services::new(Ipv4Addr::LOCALHOST.into());
        let tcp = |u: &str| BackendSpec::Tcp {
            upstream: u.to_string(),
        };

        // The startup file declares the whole set at once, so two entries for
        // one port is a mistake.
        services.register_new(9, tcp("x:9")).unwrap();
        assert!(services.register_new(9, tcp("y:9")).is_err());
        assert_eq!(services.describe(9), "tcp x:9");

        // `expose` is a live command, so re-declaring is a redirect.
        services.register(9, tcp("y:9")).unwrap();
        assert_eq!(services.describe(9), "tcp y:9");

        assert_eq!(services.ports(), vec![9]);
        assert!(services.contains(9));
        assert!(services.remove(9));
        assert!(!services.remove(9));
        assert_eq!(services.describe(9), "default 127.0.0.1:9");
    }

    #[test]
    fn backends_describe_themselves_for_list() {
        let services = Services::new(Ipv4Addr::LOCALHOST.into());
        services
            .register(
                80,
                BackendSpec::Http {
                    routes: vec![route(None, Some("/a"), "a.internal:80")],
                },
            )
            .unwrap();
        services
            .register(
                81,
                BackendSpec::Http {
                    routes: vec![
                        route(None, Some("/a"), "a.internal:80"),
                        route(None, Some("/b"), "b.internal:80"),
                    ],
                },
            )
            .unwrap();
        services
            .register(
                5432,
                BackendSpec::Tcp {
                    upstream: "db.internal:5432".into(),
                },
            )
            .unwrap();
        services
            .register(
                6000,
                BackendSpec::Unix {
                    path: "/run/docker.sock".into(),
                },
            )
            .unwrap();

        assert_eq!(services.describe(80), "http (1 route)");
        assert_eq!(services.describe(81), "http (2 routes)");
        assert_eq!(services.describe(5432), "tcp db.internal:5432");
        assert_eq!(services.describe(6000), "unix /run/docker.sock");
    }

    #[test]
    fn registering_an_invalid_backend_leaves_the_registry_untouched() {
        let services = Services::new(Ipv4Addr::LOCALHOST.into());
        assert!(services
            .register(80, BackendSpec::Http { routes: vec![] })
            .is_err());
        assert!(!services.contains(80));
    }

    #[test]
    fn synthetic_addrs_are_stable_distinct_and_in_cgnat() {
        let a = iroh::SecretKey::generate(&mut rand::rng()).public();
        let b = iroh::SecretKey::generate(&mut rand::rng()).public();

        assert_eq!(synthetic_addr(&a), synthetic_addr(&a));
        assert_ne!(synthetic_addr(&a), synthetic_addr(&b));

        let PingoraSockAddr::Inet(std::net::SocketAddr::V4(v4)) = synthetic_addr(&a) else {
            panic!("expected an IPv4 inet address");
        };
        let o = v4.ip().octets();
        assert_eq!(o[0], 100);
        assert!(
            (64..128).contains(&o[1]),
            "not in 100.64.0.0/10: {}",
            v4.ip()
        );
    }
}
