use anyhow::Result;
use clap::{ArgAction, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use std::io::IsTerminal;
use std::net::IpAddr;
use std::path::PathBuf;

mod client;
mod daemon;
mod enroll;
mod grants;
mod peer;
mod protocol;
mod schema;
mod service;
mod tunnel;

/// How results are written to stdout.
#[derive(Copy, Clone, PartialEq, Eq, Debug, ValueEnum)]
#[value(rename_all = "lower")]
pub enum Format {
    /// For a human: bare values and small tables
    Text,
    /// For a program: a stable object per command
    Json,
}

#[derive(Parser)]
#[command(
    name = "ig",
    about = "Reach a machine's internal network from outside it",
    long_about = "Reach a machine's internal network from outside it.\n\n\
        One daemon per machine. Grant a port to a peer's key and it appears on \
        that peer's localhost. Declare what a port is backed by -- a reverse \
        proxy, a tcp forward, or a unix socket -- and the connection is made by \
        the machine that can reach it.\n\n\
        Non-interactive throughout: no command reads stdin or prompts, so every \
        operation completes in one call. See docs/CONTRACT.md for exit codes \
        and JSON shapes.",
    version
)]
struct Cli {
    /// Path to the daemon's control socket
    #[arg(long, global = true, default_value = "/tmp/ig.sock", env = "IG_SOCKET")]
    socket: PathBuf,

    /// Output format for results on stdout
    #[arg(
        long,
        global = true,
        value_enum,
        default_value = "text",
        env = "IG_FORMAT"
    )]
    format: Format,

    /// Suppress status chatter on stderr; results and errors still print
    #[arg(long, short, global = true, env = "IG_QUIET")]
    quiet: bool,

    /// Never prompt or read stdin. Already the default -- accepted so scripts
    /// can assert it, and reserved against any future prompt.
    #[arg(long, global = true, action = ArgAction::SetTrue)]
    no_input: bool,

    /// Print the whole command tree as JSON and exit
    #[arg(long, exclusive = true)]
    dump_schema: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Start the daemon
    Daemon(Box<DaemonArgs>),

    /// Print this daemon's endpoint id, which is what peers dial
    Id,

    /// Show peers, exposed ports, and local bindings
    Status,

    /// Peers: who may talk to this daemon
    Peer {
        #[command(subcommand)]
        cmd: PeerCmd,
    },

    /// Ports: what this daemon serves, and where a peer's ports land locally
    Port {
        #[command(subcommand)]
        cmd: PortCmd,
    },

    /// Print a shell completion script
    Completion {
        /// Shell to generate for
        #[arg(value_enum)]
        shell: Shell,
    },
}

#[derive(Parser)]
pub struct DaemonArgs {
    /// Where ports with no declared service forward to
    #[arg(long, default_value = "127.0.0.1")]
    pub host: IpAddr,
    /// Add peer(s) on startup
    #[arg(short = 'a', long = "add", value_name = "TICKET")]
    pub peers: Vec<String>,
    /// Expose port(s) to the --add peers on startup (repeat or comma-separate)
    #[arg(
        short = 'e',
        long = "expose",
        value_delimiter = ',',
        value_name = "PORT"
    )]
    pub ports: Vec<u16>,
    /// Path to the daemon's secret key (created if missing).
    /// Defaults to $XDG_STATE_HOME/ig/key (~/.local/state/ig/key)
    #[arg(long = "key", value_name = "PATH")]
    pub key_path: Option<PathBuf>,
    /// Read the one-time enrollment token to present from this file.
    /// Prefer this over --enroll: an argv value is visible in the process
    /// table and lands in shell history.
    #[arg(long = "enroll-file", value_name = "PATH", conflicts_with = "enroll")]
    pub enroll_file: Option<PathBuf>,
    /// One-time enrollment token to present to the --add peers.
    /// Leaks through argv; prefer --enroll-file.
    #[arg(long, value_name = "TOKEN")]
    pub enroll: Option<String>,
    /// Bind a peer's port to a different local port, as REMOTE:LOCAL
    /// (repeatable). Same as `ig port bind`, applied at startup.
    #[arg(long = "bind", value_name = "REMOTE:LOCAL", value_parser = parse_bind)]
    pub binds: Vec<(u16, u16)>,
    /// Declare what exposed ports are backed by, from a TOML file (repeatable)
    #[arg(long = "service", value_name = "FILE")]
    pub services: Vec<PathBuf>,
}

#[derive(Subcommand)]
pub enum PeerCmd {
    /// Connect to a peer and start exchanging port announcements
    Add {
        /// The peer's ticket, as printed by `ig id`
        ticket: String,
        /// Validate without connecting
        #[arg(long)]
        dry_run: bool,
    },
    /// Disconnect from a peer and drop its pin
    Rm {
        /// The peer's ticket
        ticket: String,
        /// Validate without disconnecting
        #[arg(long)]
        dry_run: bool,
    },
    /// List known peers and the ports they expose to us
    Ls,
    /// Authorize a peer by key, with no token (host-attested enrollment)
    Pin {
        /// The peer's key, e.g. reported by the workload over vsock
        key: String,
        /// Label to pin the peer under
        #[arg(long)]
        label: String,
        /// Validate without pinning
        #[arg(long)]
        dry_run: bool,
    },
    /// Mint a one-time enrollment token, valid 5 minutes
    Token {
        /// Label to pin the enrolling peer under
        #[arg(long)]
        label: String,
    },
}

#[derive(Subcommand)]
pub enum PortCmd {
    /// Grant a port to peers, and optionally declare what serves it
    Expose {
        port: u16,
        /// Peer key(s) to grant to; defaults to every known peer
        #[arg(long = "to", value_name = "KEY")]
        to: Vec<String>,
        /// Serve it by forwarding to any host:port this machine can reach
        #[arg(long, group = "backend", value_name = "HOST:PORT")]
        upstream: Option<String>,
        /// Serve it by forwarding to a Unix socket on this machine
        #[arg(long, group = "backend", value_name = "PATH")]
        unix: Option<PathBuf>,
        /// Serve it by reverse-proxying, using the [[route]] table in FILE
        #[arg(long, group = "backend", value_name = "FILE")]
        routes: Option<PathBuf>,
        /// Report what would change, and change nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// Revoke grants for a port. Revoking the last one retires its service.
    Unexpose {
        port: u16,
        /// Revoke only this peer's grant; defaults to every grant for the port
        #[arg(long = "to", value_name = "KEY")]
        to: Option<String>,
        /// Report what would change, and change nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// List the ports this daemon exposes and what serves each
    Ls,
    /// Bind a peer's port to a different local port, for when the number it
    /// announced is already taken here
    Bind {
        /// The port as the peer announces it
        port: u16,
        /// Local port to listen on; 0 asks the OS for any free port
        #[arg(long, required_unless_present = "clear")]
        local: Option<u16>,
        /// Drop the remap and go back to binding the announced port
        #[arg(long, conflicts_with = "local")]
        clear: bool,
        /// Report what would change, and change nothing
        #[arg(long)]
        dry_run: bool,
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

/// Read an enrollment token from a file, trimming the newline a shell redirect
/// leaves behind.
fn read_token(path: &std::path::Path) -> Result<String> {
    let token = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("cannot read enrollment token {}: {e}", path.display()))?;
    Ok(token.trim().to_string())
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // The exit code is the contract; see docs/CONTRACT.md.
    let code = match run(cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            protocol::Failure::kind_of(&e).exit_code()
        }
    };
    std::process::exit(code);
}

async fn run(cli: Cli) -> Result<i32> {
    if cli.dump_schema {
        println!("{}", schema::dump(&Cli::command())?);
        return Ok(0);
    }

    let Some(command) = cli.command else {
        Cli::command().print_help()?;
        return Ok(protocol::ErrorKind::Invalid.exit_code());
    };

    match command {
        Command::Completion { shell } => {
            let mut cmd = Cli::command();
            let name = cmd.get_name().to_string();
            clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
            Ok(0)
        }
        Command::Daemon(args) => {
            init_logging(cli.quiet);
            let enroll = match &args.enroll_file {
                Some(path) => Some(read_token(path)?),
                None => args.enroll.clone(),
            };
            daemon::run(
                &cli.socket,
                daemon::Options {
                    host: args.host,
                    peers: args.peers,
                    ports: args.ports,
                    key_path: args.key_path,
                    enroll,
                    binds: args.binds,
                    service_configs: args.services,
                },
            )
            .await?;
            Ok(0)
        }
        other => client::run(&cli.socket, other, cli.format, cli.quiet).await,
    }
}

/// Colour only when a human is watching stderr. Piping the daemon's logs into
/// a file should not fill it with escape codes.
fn init_logging(quiet: bool) {
    let directive = if quiet { "ig=warn" } else { "ig=info" };
    let filter = tracing_subscriber::EnvFilter::from_default_env()
        .add_directive(directive.parse().expect("static directive is valid"));
    tracing_subscriber::fmt()
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .init();
}
