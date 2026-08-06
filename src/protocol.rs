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
        dry_run: bool,
    },
    RemovePeer {
        ticket: String,
        dry_run: bool,
    },
    /// Grant `port` to `to`; empty `to` grants to all currently known peers.
    /// `backend` declares what the port is served by; absent keeps whatever is
    /// already declared, or the `--host:<port>` default.
    Expose {
        port: u16,
        to: Vec<String>,
        backend: Option<BackendSpec>,
        dry_run: bool,
    },
    /// Bind a peer's `port` to a different local port. `local` of None clears
    /// the override, and 0 asks the OS for any free port.
    Bind {
        port: u16,
        local: Option<u16>,
        dry_run: bool,
    },
    /// Revoke grants for `port`; `to` limits it to one grantee
    Unexpose {
        port: u16,
        to: Option<String>,
        dry_run: bool,
    },
    /// Everything: peers, exposed ports, bindings
    List,
    /// Peers only
    ListPeers,
    /// Exposed ports and their grants only
    ListPorts,
    Ticket,
    GrantToken {
        label: String,
    },
    /// Pin a peer's key under a label without a token (host-attested)
    Pin {
        key: String,
        label: String,
        dry_run: bool,
    },
}

/// Why a request failed, in the terms a caller can act on.
///
/// This is what the client turns into an exit code. Without it every caller
/// has to classify failures by grepping the message, which breaks the moment
/// the wording changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The request was malformed: an unparseable key, a bad backend spec
    Invalid,
    /// The thing named does not exist, or could not be reached
    NotFound,
    /// It already exists, or something else holds it
    Conflict,
    /// Refused on authorization grounds
    Denied,
    /// The daemon could not be reached at all
    Unavailable,
    /// Anything else
    Internal,
}

impl ErrorKind {
    /// Exit code for this failure. 0-9 are kept portable; see docs/CONTRACT.md.
    pub fn exit_code(self) -> i32 {
        match self {
            ErrorKind::Invalid => 2,
            ErrorKind::NotFound => 3,
            ErrorKind::Denied => 4,
            ErrorKind::Conflict => 5,
            ErrorKind::Unavailable => 7,
            ErrorKind::Internal => 1,
        }
    }
}

/// An error carrying its kind, so the daemon classifies once and every caller
/// agrees. Attached to `anyhow` chains and recovered by downcast.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct Failure {
    pub kind: ErrorKind,
    pub message: String,
}

impl Failure {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// The kind of an arbitrary error: whatever it was tagged with, or
    /// Internal if it was never classified.
    pub fn kind_of(err: &anyhow::Error) -> ErrorKind {
        err.chain()
            .find_map(|e| e.downcast_ref::<Failure>())
            .map(|f| f.kind)
            .unwrap_or(ErrorKind::Internal)
    }
}

/// Response from daemon to CLI client
#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    /// An action completed (or, with `dry_run`, would have). `detail` is a
    /// one-line human summary; it is not a stable field to parse.
    Done {
        dry_run: bool,
        detail: String,
    },
    Ticket(String),
    List(ListInfo),
    Token(String),
    Error {
        kind: ErrorKind,
        message: String,
    },
}

impl Response {
    /// An action that ran (or would have, under `dry_run`).
    pub fn done(dry_run: bool, detail: impl Into<String>) -> Self {
        Response::Done {
            dry_run,
            detail: detail.into(),
        }
    }

    pub fn error(err: anyhow::Error) -> Self {
        Response::Error {
            kind: Failure::kind_of(&err),
            message: format!("{err:#}"),
        }
    }
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
    /// Remaps declared by `port bind`, whether or not a peer is currently
    /// offering the port. A rule with no live listener is invisible in
    /// `bindings`, which made a `bind` against an offline peer impossible to
    /// confirm.
    #[serde(default)]
    pub remaps: Vec<RemapInfo>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RemapInfo {
    /// The port as a peer announces it
    pub port: u16,
    /// Where it is told to land here
    pub local: u16,
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
