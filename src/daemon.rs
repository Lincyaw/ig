//! Daemon - manages iroh endpoint, peers, and tunnels.

use crate::enroll::{Pins, Tokens};
use crate::grants::Grants;
use crate::peer::PeerManager;
use crate::protocol::{
    ErrorKind, ExposedInfo, Failure, GrantInfo, ListInfo, RemapInfo, Request, Response, ALPN,
};
use crate::service::Services;
use crate::state::{State, Store};
use anyhow::{anyhow, Context, Result};
use iroh::{Endpoint, EndpointId, SecretKey};
use std::collections::BTreeSet;
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

pub struct Daemon {
    /// The iroh endpoint
    endpoint: Endpoint,
    /// Directed grants: which port is exposed to which peer
    grants: Arc<RwLock<Grants>>,
    /// Connected peers
    peers: Arc<PeerManager>,
    /// Enrollment tokens minted by grant-token
    tokens: Arc<Tokens>,
    /// What every service registers against, and what `list` describes from
    services: Arc<Services>,
    /// Where grants, backends, and binds are written so a restart keeps them
    store: Store,
}

/// Default key location: $XDG_STATE_HOME/ig/key (~/.local/state/ig/key)
fn default_key_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("state"))
        })
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("ig").join("key")
}

/// Claim the control socket, or refuse to start.
///
/// Taking over a path a live daemon is bound to is worse than failing: unlinking
/// it does not stop that process, which keeps its peers and its bound ports and
/// simply becomes unreachable by path. What looked like a restart is then two
/// daemons serving the same ports, and `ig status` only ever shows one of them.
/// connect(2) is what separates that from the socket a crash left behind -- a
/// dead one refuses.
///
/// The bound socket is 0600 because anything that can reach it can expose ports
/// and mint enrollment tokens. The default path lives in /tmp, where the umask
/// would otherwise leave the control plane open to every local user -- which
/// would make a rather poor showing for a tool whose whole claim is that a port
/// goes to exactly the peer you name.
fn bind_control_socket(path: &Path) -> Result<UnixListener> {
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => {
            return Err(Failure::new(
                ErrorKind::Conflict,
                format!(
                    "a daemon is already listening on {}; stop it first, or pass \
                     a different --socket",
                    path.display()
                ),
            )
            .into())
        }
        // Nothing there, or nothing alive behind it.
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            let _ = std::fs::remove_file(path);
        }
        Err(e) => {
            return Err(anyhow!(
                "cannot tell whether a daemon holds {}: {e}",
                path.display()
            ))
        }
    }

    let listener =
        UnixListener::bind(path).with_context(|| format!("failed to bind {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to restrict {}", path.display()))?;
    }

    Ok(listener)
}

/// Load the secret key from `path`, or generate one and persist it there.
/// The key file is 32 raw bytes, created with mode 0600.
fn load_or_create_key(path: &Path) -> Result<SecretKey> {
    if path.exists() {
        let bytes = std::fs::read(path)
            .with_context(|| format!("failed to read key file {}", path.display()))?;
        let bytes: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow!("key file {} is not 32 bytes", path.display()))?;
        return Ok(SecretKey::from_bytes(&bytes));
    }

    let key = SecretKey::generate(&mut rand::rng());

    if let Some(parent) = path.parent() {
        // 0700 on anything created here. The key beside it is 0600 already, but
        // the pin file is not secret and is still worth protecting: a pin is an
        // authorization, so somewhere another user can write one is somewhere
        // they can grant themselves a peer. A directory that already exists is
        // left as the user set it up.
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    use std::io::Write;
    let mut file = opts
        .open(path)
        .with_context(|| format!("failed to create key file {}", path.display()))?;
    file.write_all(&key.to_bytes())
        .with_context(|| format!("failed to write key file {}", path.display()))?;

    info!("generated new key at {}", path.display());
    Ok(key)
}

impl Daemon {
    pub async fn new(key_path: &Path, services: Services) -> Result<Arc<Self>> {
        let secret_key = load_or_create_key(key_path)?;

        let endpoint = Endpoint::builder(iroh::endpoint::presets::N0)
            .secret_key(secret_key)
            .alpns(vec![ALPN.to_vec()])
            .bind()
            .await
            .context("failed to create iroh endpoint")?;

        let grants = Arc::new(RwLock::new(Grants::default()));
        let tokens = Arc::new(Tokens::default());

        // Pins live next to the key: <key>.peers.json
        let pins = Pins::new(PathBuf::from(format!("{}.peers.json", key_path.display())));
        let pinned = pins.load()?;

        let services = Arc::new(services);
        let daemon = Arc::new(Self {
            peers: Arc::new(PeerManager::new(
                endpoint.clone(),
                grants.clone(),
                tokens.clone(),
                pins,
                services.clone(),
            )),
            endpoint,
            grants,
            tokens,
            services,
            store: Store::new(key_path),
        });

        for pin in pinned {
            if let Err(e) = daemon.peers.add_pinned(&pin.key, &pin.label) {
                error!("failed to load pinned peer {}: {}", pin.key, e);
            }
        }

        Ok(daemon)
    }

    pub fn ticket(&self) -> String {
        // TODO: proper ticket serialization
        self.endpoint.id().to_string()
    }

    /// Everything a restart would otherwise forget.
    async fn snapshot(&self) -> State {
        State {
            version: 0, // stamped by the store
            grants: self
                .grants
                .read()
                .await
                .all()
                .into_iter()
                .map(|(port, to)| (port, to.to_string()))
                .collect(),
            services: self.services.snapshot(),
            binds: self.peers.binds_snapshot(),
        }
    }

    /// Write the current declarations out.
    ///
    /// Called from the control socket only. A failure is returned rather than
    /// logged: the change did take effect in memory, but reporting success for
    /// something that will vanish at the next restart is precisely the bug this
    /// file exists to close.
    async fn persist(&self) -> Result<()> {
        self.store.save(&self.snapshot().await)
    }

    /// Restore what a previous run was told. Applied before any startup
    /// declaration, so `--service` and `-e` remain an overlay for this run.
    async fn restore(&self, state: State) -> Result<()> {
        for (port, spec) in state.services {
            // A --service file already claimed this port: it is what the
            // operator just edited, so it wins over what an earlier run
            // happened to be told.
            if !self.services.contains(port) {
                self.services.register(port, spec)?;
            }
        }
        {
            let mut grants = self.grants.write().await;
            for (port, key) in &state.grants {
                match parse_key(key) {
                    Ok(id) => grants.add(*port, id),
                    Err(e) => warn!("dropping unreadable grant for port {port}: {e}"),
                }
            }
        }
        for (port, local) in state.binds {
            self.peers.set_bind(port, Some(local)).await;
        }
        Ok(())
    }

    /// Grant `port` to each peer in `to` and re-announce
    pub async fn expose(&self, port: u16, to: &[EndpointId]) -> Result<()> {
        {
            let mut grants = self.grants.write().await;
            for grantee in to {
                grants.add(port, *grantee);
            }
        }
        self.peers.broadcast_grants().await;
        info!("exposed port {} to {} peer(s)", port, to.len());
        Ok(())
    }

    /// Revoke grants for `port` (all of them, or just `to`) and re-announce.
    ///
    /// A port whose last grantee goes away retires its service with it: the
    /// port is gone, and leaving a backend registered for it would be a
    /// half-deleted thing `list` cannot show. The rule is on the state, not on
    /// the command -- revoking the one remaining grantee by name reaches the
    /// same place as revoking them all, and must land the same way.
    pub async fn unexpose(&self, port: u16, to: Option<EndpointId>) -> Result<()> {
        let ungranted = {
            let mut grants = self.grants.write().await;
            grants.remove(port, to);
            !grants.ports().contains(&port)
        };
        if ungranted && self.services.remove(port) {
            info!("retired service on port {}", port);
        }
        self.peers.broadcast_grants().await;
        info!("unexposed port {}", port);
        Ok(())
    }

    pub async fn list(&self) -> ListInfo {
        let grants = self.grants.read().await;
        ListInfo {
            me: self.endpoint.id().to_string(),
            peers: self.peers.list().await,
            i_expose: grants
                .ports()
                .into_iter()
                .map(|port| ExposedInfo {
                    backend: self.services.describe(port),
                    port,
                })
                .collect(),
            grants: grants
                .all()
                .into_iter()
                .map(|(port, to)| GrantInfo {
                    port,
                    to: to.to_string(),
                })
                .collect(),
            bindings: self.peers.list_bindings().await,
            remaps: self
                .peers
                .binds_snapshot()
                .into_iter()
                .map(|(port, local)| RemapInfo { port, local })
                .collect(),
        }
    }

    /// Accept incoming peer connections
    pub async fn accept_loop(self: Arc<Self>) {
        loop {
            match self.endpoint.accept().await {
                Some(incoming) => {
                    let this = self.clone();
                    tokio::spawn(async move {
                        if let Err(e) = this.handle_incoming(incoming).await {
                            error!("error handling incoming connection: {}", e);
                        }
                    });
                }
                None => {
                    info!("endpoint closed");
                    break;
                }
            }
        }
    }

    async fn handle_incoming(&self, incoming: iroh::endpoint::Incoming) -> Result<()> {
        let conn = incoming.accept()?.await?;
        self.peers.handle_connection(conn).await
    }

    /// Handle a request from the CLI client.
    ///
    /// Every mutating arm validates fully before it changes anything, so
    /// `dry_run` can return the same verdict the real run would reach by
    /// stopping just short of the mutation. That is only trustworthy because
    /// validation comes first everywhere -- keep it that way.
    pub async fn handle_request(self: &Arc<Self>, request: Request) -> Response {
        match request {
            Request::AddPeer { ticket, dry_run } => {
                match parse_key(&ticket) {
                    Err(e) => return Response::error(e),
                    Ok(id) if dry_run => {
                        return Response::done(true, format!("connect to peer {id}"))
                    }
                    Ok(_) => {}
                }
                match self.peers.add_peer(&ticket, None).await {
                    Ok(()) => Response::done(false, format!("connected to peer {ticket}")),
                    Err(e) => Response::error(e),
                }
            }

            Request::RemovePeer { ticket, dry_run } => {
                match parse_key(&ticket) {
                    Err(e) => return Response::error(e),
                    Ok(id) if dry_run => {
                        return Response::done(true, format!("disconnect from peer {id}"))
                    }
                    Ok(_) => {}
                }
                match self.peers.remove_peer(&ticket).await {
                    // remove_peer revokes that peer's grants, so the file has
                    // to follow or a restart would hand them back.
                    Ok(()) => match self.persist().await {
                        Ok(()) => Response::done(false, format!("removed peer {ticket}")),
                        Err(e) => Response::error(e),
                    },
                    Err(e) => Response::error(e),
                }
            }

            Request::Expose {
                port,
                to,
                backend,
                dry_run,
            } => {
                // Resolve grantees before registering: a typo in --to must not
                // leave a backend declared for a port nobody can reach.
                let grantees: Result<Vec<EndpointId>> = if to.is_empty() {
                    let ids = self.peers.peer_ids();
                    if ids.is_empty() {
                        Err(Failure::new(
                            ErrorKind::Invalid,
                            "no peers to grant to; use --to <key>",
                        )
                        .into())
                    } else {
                        Ok(ids)
                    }
                } else {
                    to.iter().map(|k| parse_key(k)).collect()
                };
                let grantees = match grantees {
                    Ok(g) => g,
                    Err(e) => return Response::error(e),
                };

                // Validating the spec here is what lets --dry-run reject a bad
                // route table without registering it.
                if let Some(spec) = &backend {
                    if let Err(e) = spec.validate(port) {
                        return Response::error(e);
                    }
                }
                let served = match &backend {
                    Some(spec) => spec.describe(),
                    None => self.services.describe(port),
                };
                let detail = format!(
                    "expose {port} to {} peer(s), served by {served}",
                    grantees.len()
                );
                if dry_run {
                    return Response::done(true, detail);
                }

                if let Some(spec) = backend {
                    if let Err(e) = self.services.register(port, spec) {
                        return Response::error(e);
                    }
                }
                match self.expose(port, &grantees).await {
                    Ok(()) => match self.persist().await {
                        Ok(()) => Response::done(false, detail),
                        Err(e) => Response::error(e),
                    },
                    Err(e) => Response::error(e),
                }
            }

            Request::Unexpose { port, to, dry_run } => {
                let grantee = match to.as_deref().map(parse_key).transpose() {
                    Ok(g) => g,
                    Err(e) => return Response::error(e),
                };
                let detail = match grantee {
                    Some(g) => format!("revoke {port} from {g}"),
                    None => format!("revoke every grant for {port}"),
                };
                if dry_run {
                    return Response::done(true, detail);
                }
                match self.unexpose(port, grantee).await {
                    Ok(()) => match self.persist().await {
                        Ok(()) => Response::done(false, detail),
                        Err(e) => Response::error(e),
                    },
                    Err(e) => Response::error(e),
                }
            }

            Request::Bind {
                port,
                local,
                dry_run,
            } => {
                let detail = match local {
                    Some(0) => format!("bind peer port {port} on a free local port"),
                    Some(l) => format!("bind peer port {port} on local port {l}"),
                    None => format!("bind peer port {port} on {port} again"),
                };
                if dry_run {
                    return Response::done(true, detail);
                }
                self.peers.set_bind(port, local).await;
                match self.persist().await {
                    Ok(()) => Response::done(false, detail),
                    Err(e) => Response::error(e),
                }
            }

            Request::Pin {
                key,
                label,
                dry_run,
            } => {
                match parse_key(&key) {
                    Err(e) => return Response::error(e),
                    Ok(id) if dry_run => {
                        return Response::done(true, format!("pin {id} as \"{label}\""))
                    }
                    Ok(_) => {}
                }
                match self.peers.pin_peer(&key, &label) {
                    Ok(()) => Response::done(false, format!("pinned {key} as \"{label}\"")),
                    Err(e) => Response::error(e),
                }
            }

            Request::List | Request::ListPeers | Request::ListPorts => {
                Response::List(self.list().await)
            }
            Request::Ticket => Response::Ticket(self.ticket()),
            Request::GrantToken { label } => Response::Token(self.tokens.mint(label)),
        }
    }
}

/// Parse an endpoint id, classified so a typo exits 2 rather than 1.
fn parse_key(key: &str) -> Result<EndpointId> {
    key.parse()
        .map_err(|e| Failure::new(ErrorKind::Invalid, format!("invalid peer key: {e}")).into())
}

/// Everything `ig daemon` was invoked with.
pub struct Options {
    /// Fallback address for ports with no service declared
    pub host: IpAddr,
    /// Peers to add on startup
    pub peers: Vec<String>,
    /// Ports to expose to those peers on startup
    pub ports: Vec<u16>,
    /// Where the secret key lives
    pub key_path: Option<PathBuf>,
    /// One-time enrollment token to present to `peers`
    pub enroll: Option<String>,
    /// REMOTE:LOCAL port remaps to apply on startup
    pub binds: Vec<(u16, u16)>,
    /// Service declarations to apply on startup
    pub service_configs: Vec<PathBuf>,
}

/// Run the daemon
pub async fn run(socket_path: &Path, opts: Options) -> Result<()> {
    let Options {
        host,
        peers,
        ports,
        key_path,
        enroll,
        binds,
        service_configs,
    } = opts;
    // Apply the startup file before touching anything else: it makes the same
    // declarations `expose` makes, and a bad one should fail the daemon at
    // startup rather than on the first request that hits it.
    let services = Services::new(host);
    for path in &service_configs {
        for (port, spec) in crate::service::load_service_file(path)? {
            services
                .register_new(port, spec)
                .with_context(|| format!("in service config {}", path.display()))?;
        }
    }
    // Claimed before anything else runs. The socket is the only thing making
    // two daemons on one path mutually exclusive, so binding it first is what
    // keeps the window between "is anyone there" and "I am now there" from
    // spanning the whole of startup. Nothing is accepted until the loop below,
    // so an early client waits rather than talking to a half-built daemon.
    let listener = bind_control_socket(socket_path)?;

    let key_path = key_path.unwrap_or_else(default_key_path);
    // Read before the endpoint is built, so a file this build cannot understand
    // stops the daemon rather than being half-applied and written back.
    let restored = Store::new(&key_path).load()?;
    let daemon = Daemon::new(&key_path, services).await?;

    // What a previous run was told, then this run's declarations over the top.
    // A --service file already applied above keeps its port; everything else
    // comes back exactly as it was left.
    if !restored.is_empty() {
        info!(
            "restored {} grant(s), {} service(s), {} bind(s)",
            restored.grants.len(),
            restored.services.len(),
            restored.binds.len()
        );
        daemon.restore(restored).await?;
    }

    for (remote, local) in binds {
        daemon.peers.set_bind(remote, Some(local)).await;
    }

    // After restore, so a service that came back from the file is granted too.
    let service_ports = daemon.services.ports();

    println!("Ticket: {}", daemon.ticket());
    info!("daemon started, host={}, key={}", host, key_path.display());

    // -e ports are granted to the -a peers: expose these ports to those
    // peers, and to no one else. Service ports join them -- declaring a service
    // is already a statement that the port should be served.
    let grantees: Vec<EndpointId> = peers.iter().filter_map(|t| t.parse().ok()).collect();
    let ports: BTreeSet<u16> = ports.into_iter().chain(service_ports).collect();
    if !ports.is_empty() && grantees.is_empty() {
        warn!("nothing to grant to: ports are granted to no one; use -a or expose --to");
    }
    for &port in &ports {
        daemon.expose(port, &grantees).await?;
    }

    // Add peers specified on command line, presenting the enroll token if given
    for ticket in &peers {
        match daemon.peers.add_peer(ticket, enroll.clone()).await {
            Ok(()) => {
                info!("added peer {}", ticket);
            }
            Err(e) => {
                error!("failed to add peer {}: {}", ticket, e);
            }
        }
    }

    // Announce grants to the newly added peers
    if !peers.is_empty() {
        daemon.peers.broadcast_grants().await;
    }

    // Start accepting peer connections
    let accept_daemon = daemon.clone();
    tokio::spawn(async move {
        accept_daemon.accept_loop().await;
    });

    info!("listening on {:?}", socket_path);

    loop {
        let (stream, _) = listener.accept().await?;
        let daemon = daemon.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(stream, daemon).await {
                error!("client error: {}", e);
            }
        });
    }
}

async fn handle_client(stream: UnixStream, daemon: Arc<Daemon>) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    reader.read_line(&mut line).await?;
    let request: Request = serde_json::from_str(&line)?;

    let response = daemon.handle_request(request).await;
    let response_json = serde_json::to_string(&response)?;

    writer.write_all(response_json.as_bytes()).await?;
    writer.write_all(b"\n").await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_persists_across_loads() {
        let dir = std::env::temp_dir().join(format!("ig-key-test-{}", std::process::id()));
        let path = dir.join("key");

        let first = load_or_create_key(&path).unwrap();
        let second = load_or_create_key(&path).unwrap();
        assert_eq!(first.to_bytes(), second.to_bytes());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_malformed_key_file() {
        let dir = std::env::temp_dir().join(format!("ig-badkey-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("key");
        std::fs::write(&path, b"too short").unwrap();

        assert!(load_or_create_key(&path).is_err());

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
