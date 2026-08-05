//! Protocol definitions for daemon<->client and peer<->peer communication.

use crate::service::BackendSpec;
use serde::{Deserialize, Serialize};

/// ALPN protocol identifier.
///
/// Deliberately still pai-sho's: the peer-to-peer messages in this file are
/// unchanged from upstream, so the two remain wire compatible. Everything this
/// fork adds -- what a port is backed by, where it binds locally -- is decided
/// on one side and never crosses the link. Change this only when that stops
/// being true.
pub const ALPN: &[u8] = b"PAI_SHO/1";

// ============================================================================
// Client <-> Daemon (over Unix socket)
// ============================================================================

/// Request from CLI client to daemon
#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    AddPeer {
        ticket: String,
    },
    RemovePeer {
        ticket: String,
    },
    /// Grant `port` to `to`; empty `to` grants to all currently known peers.
    /// `backend` declares what the port is served by; absent keeps whatever is
    /// already declared, or the `--host:<port>` default.
    Expose {
        port: u16,
        to: Vec<String>,
        backend: Option<BackendSpec>,
    },
    /// Bind a peer's `port` to a different local port. `local` of None clears
    /// the override, and 0 asks the OS for any free port.
    Bind {
        port: u16,
        local: Option<u16>,
    },
    /// Revoke grants for `port`; `to` limits it to one grantee
    Unexpose {
        port: u16,
        to: Option<String>,
    },
    List,
    Ticket,
    GrantToken {
        label: String,
    },
    /// Pin a peer's key under a label without a token (host-attested)
    Pin {
        key: String,
        label: String,
    },
}

/// Response from daemon to CLI client
#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Ok,
    Ticket(String),
    List(ListInfo),
    Token(String),
    Error(String),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ListInfo {
    /// This node's own key (its ticket)
    pub me: String,
    pub peers: Vec<PeerInfo>,
    /// Ports this node exposes, and what each is served by
    pub i_expose: Vec<ExposedInfo>,
    /// Who each port is granted to, one row per (port, grantee)
    pub grants: Vec<GrantInfo>,
    pub bindings: Vec<BindingInfo>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ExposedInfo {
    pub port: u16,
    /// What serves it: "http (3 routes)", "tcp db.internal:5432",
    /// "unix /run/docker.sock", or "default 127.0.0.1:3002"
    pub backend: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GrantInfo {
    pub port: u16,
    /// Key of the peer this port is granted to
    pub to: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PeerInfo {
    pub key: String,
    /// Label assigned at enrollment (absent for peers added by ticket)
    pub label: Option<String>,
    pub online: bool,
    /// Ports this peer exposes to us
    pub they_expose: Vec<u16>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BindingInfo {
    /// The port the peer announced
    pub port: u16,
    /// The local port it is actually listening on, which differs from `port`
    /// when overridden by `bind` or when the requested port was taken
    pub local: u16,
    /// Key of the peer this local port tunnels to
    pub peer: String,
}

// ============================================================================
// Peer <-> Peer (over iroh QUIC)
// ============================================================================

/// Message sent between peers over iroh
#[derive(Debug, Serialize, Deserialize)]
pub enum PeerMessage {
    /// Announce exposed ports (sent on connect and when ports change)
    ExposedPorts(Vec<u16>),
    /// Request to connect to a specific port
    Connect { port: u16 },
    /// Present a one-time enrollment token (sent on connect by `--enroll`)
    Enroll { token: String },
    /// Error response
    Error(String),
}
