use anyhow::Result;
use clap::{Parser, Subcommand};
use std::net::IpAddr;

mod client;
mod daemon;
mod enroll;
mod grants;
mod peer;
mod protocol;
mod service;
mod tunnel;

#[derive(Parser)]
#[clap(
    name = "iroh-gate",
    about = "Reach a machine's internal network from outside it",
    version
)]
struct Cli {
    /// Path to Unix socket
    #[arg(long, default_value = "/tmp/iroh-gate.sock")]
    socket: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Start the daemon
    Daemon {
        /// Host address for forwarding exposed ports
        #[arg(long, default_value = "127.0.0.1")]
        host: IpAddr,
        /// Add peer(s) on startup
        #[arg(short = 'a', long = "add")]
        peers: Vec<String>,
        /// Expose port(s) on startup (repeat or comma-separate)
        #[arg(short = 'e', long = "expose", value_delimiter = ',')]
        ports: Vec<u16>,
        /// Path to the daemon's secret key (created if missing).
        /// Defaults to $XDG_STATE_HOME/iroh-gate/key (~/.local/state/iroh-gate/key)
        #[arg(long = "key")]
        key_path: Option<std::path::PathBuf>,
        /// One-time enrollment token to present to added peers
        #[arg(long)]
        enroll: Option<String>,
        /// Bind a peer's port to a different local port, as REMOTE:LOCAL
        /// (repeatable). Same as the `bind` command, applied at startup.
        #[arg(long = "bind", value_name = "REMOTE:LOCAL", value_parser = parse_bind)]
        binds: Vec<(u16, u16)>,
        /// Declare what exposed ports are backed by, from a TOML file
        /// (repeatable). A service port is granted like any other, but is
        /// served by an HTTP reverse proxy, a forward to any host:port this
        /// machine can reach, or a forward to a local Unix socket -- rather
        /// than the default forward to --host:<port>.
        #[arg(long = "service", value_name = "FILE")]
        services: Vec<std::path::PathBuf>,
    },

    /// Add a peer and connect to it
    AddPeer {
        /// Peer's ticket (endpoint ID)
        ticket: String,
    },

    /// Remove a peer
    RemovePeer {
        /// Peer's ticket
        ticket: String,
    },

    /// Expose a port to specific peers (a directed grant), and optionally
    /// declare what serves it. With no backend flag the port keeps whatever
    /// backend it already had, or forwards to --host:<port>.
    Expose {
        port: u16,
        /// Peer key(s) to grant the port to; defaults to all known peers
        #[arg(long = "to")]
        to: Vec<String>,
        /// Serve it by forwarding to any host:port this machine can reach,
        /// e.g. db.internal:5432. Resolved here, so internal names work.
        #[arg(long, group = "backend", value_name = "HOST:PORT")]
        upstream: Option<String>,
        /// Serve it by forwarding to a Unix socket on this machine
        #[arg(long, group = "backend", value_name = "PATH")]
        unix: Option<std::path::PathBuf>,
        /// Serve it by reverse-proxying, using the [[route]] table in FILE
        #[arg(long, group = "backend", value_name = "FILE")]
        routes: Option<std::path::PathBuf>,
    },

    /// Bind a peer's exposed port to a different local port, for when the
    /// number the peer chose is already taken here. Takes effect immediately.
    Bind {
        /// The port as the peer announces it
        port: u16,
        /// Local port to listen on; 0 asks the OS for any free port
        #[arg(long, required_unless_present = "clear")]
        local: Option<u16>,
        /// Drop the remap and go back to binding the announced port
        #[arg(long, conflicts_with = "local")]
        clear: bool,
    },

    /// Revoke grants for a port
    Unexpose {
        port: u16,
        /// Revoke only this peer's grant; defaults to every grant for the port
        #[arg(long = "to")]
        to: Option<String>,
    },

    /// List peers, exposed ports, and bindings
    List,

    /// Print daemon's ticket
    Ticket,

    /// Mint a one-time enrollment token (valid 5 minutes)
    GrantToken {
        /// Label to pin the enrolling peer under
        #[arg(long)]
        label: String,
    },

    /// Pin a peer by its key under a label, no token (host-attested
    /// enrollment). The key is authorized when the peer dials in; nothing
    /// secret travels into the workload. See
    /// docs/adr/0003-host-attested-enrollment.md.
    Pin {
        /// Peer's key (endpoint ID), e.g. reported by the workload over vsock
        key: String,
        /// Label to pin the peer under
        #[arg(long)]
        label: String,
    },
}

/// Parse a `--bind REMOTE:LOCAL` pair, so a typo is a usage error from clap
/// rather than a failure part-way through daemon startup.
fn parse_bind(arg: &str) -> Result<(u16, u16), String> {
    let (remote, local) = arg
        .split_once(':')
        .ok_or_else(|| format!("wants REMOTE:LOCAL, got {arg}"))?;
    Ok((
        remote
            .parse()
            .map_err(|_| format!("bad remote port {remote}"))?,
        local
            .parse()
            .map_err(|_| format!("bad local port {local}"))?,
    ))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("iroh_gate=info".parse()?),
        )
        .init();

    let cli = Cli::parse();

    let socket_path = std::path::Path::new(&cli.socket);

    match cli.command {
        Command::Daemon {
            host,
            peers,
            ports,
            key_path,
            enroll,
            binds,
            services,
        } => {
            daemon::run(
                socket_path,
                daemon::Options {
                    host,
                    peers,
                    ports,
                    key_path,
                    enroll,
                    binds,
                    service_configs: services,
                },
            )
            .await?;
        }
        _ => {
            client::send_command(socket_path, cli.command).await?;
        }
    }

    Ok(())
}
