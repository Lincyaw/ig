//! CLI client - sends a command to the daemon and renders the reply.
//!
//! Two rules shape everything here:
//!
//! - **stdout carries results, stderr carries everything else.** `ig ... >/dev/null`
//!   must isolate the data, so status lines never go to stdout. A command whose
//!   result is an action rather than a value writes nothing to stdout in text
//!   mode.
//! - **The exit code says what happened.** The daemon classifies each failure
//!   once, and every caller maps it the same way, so nobody has to grep the
//!   message to decide whether to retry.

use crate::protocol::{ErrorKind, Failure, ListInfo, Request, Response};
use crate::service::{self, BackendSpec};
use crate::{Command, Format, PeerCmd, PortCmd};
use anyhow::Result;
use serde_json::json;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub async fn run(socket_path: &Path, command: Command, format: Format, quiet: bool) -> Result<i32> {
    let request = build_request(command)?;
    let view = ListView::of(&request);
    let response = send(socket_path, &request).await?;
    render(response, view, format, quiet)
}

/// Which slice of `ListInfo` a listing command wants. The daemon always returns
/// the whole picture; picking a view here lets one request type serve `status`,
/// `peer ls`, and `port ls`.
#[derive(Clone, Copy, PartialEq)]
enum ListView {
    All,
    Peers,
    Ports,
    None,
}

impl ListView {
    fn of(request: &Request) -> Self {
        match request {
            Request::List => ListView::All,
            Request::ListPeers => ListView::Peers,
            Request::ListPorts => ListView::Ports,
            _ => ListView::None,
        }
    }
}

fn build_request(command: Command) -> Result<Request> {
    Ok(match command {
        Command::Id => Request::Ticket,
        Command::Status => Request::List,

        Command::Peer { cmd } => match cmd {
            PeerCmd::Add { ticket, dry_run } => Request::AddPeer { ticket, dry_run },
            PeerCmd::Rm { ticket, dry_run } => Request::RemovePeer { ticket, dry_run },
            PeerCmd::Ls => Request::ListPeers,
            PeerCmd::Pin {
                key,
                label,
                dry_run,
            } => Request::Pin {
                key,
                label,
                dry_run,
            },
            PeerCmd::Token { label } => Request::GrantToken { label },
        },

        Command::Port { cmd } => match cmd {
            PortCmd::Expose {
                port,
                to,
                upstream,
                unix,
                routes,
                dry_run,
            } => {
                // The route table is parsed here, not in the daemon, so a bad
                // table is reported against the file the user just pointed at.
                let backend = match (upstream, unix, routes) {
                    (Some(upstream), None, None) => Some(BackendSpec::Tcp { upstream }),
                    (None, Some(path), None) => Some(BackendSpec::Unix { path }),
                    (None, None, Some(file)) => Some(BackendSpec::Http {
                        routes: service::load_routes(&file)?,
                    }),
                    _ => None,
                };
                Request::Expose {
                    port,
                    to,
                    backend,
                    dry_run,
                }
            }
            PortCmd::Unexpose { port, to, dry_run } => Request::Unexpose { port, to, dry_run },
            PortCmd::Ls => Request::ListPorts,
            // clap guarantees exactly one of --local / --clear is present.
            PortCmd::Bind {
                port,
                local,
                clear,
                dry_run,
            } => Request::Bind {
                port,
                local: if clear { None } else { local },
                dry_run,
            },
        },

        Command::Daemon(_)
        | Command::Completion { .. }
        | Command::Autostart { .. }
        | Command::Upgrade { .. } => {
            unreachable!("handled before reaching the client")
        }
    })
}

async fn send(socket_path: &Path, request: &Request) -> Result<Response> {
    // A missing socket is an environment problem, not a bad request: its own
    // exit code lets a caller tell "start the daemon" from "fix your arguments".
    let stream = UnixStream::connect(socket_path).await.map_err(|e| {
        Failure::new(
            ErrorKind::Unavailable,
            format!(
                "cannot reach the daemon at {} ({e}); start it with `ig daemon`",
                socket_path.display()
            ),
        )
    })?;

    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    writer
        .write_all(serde_json::to_string(request)?.as_bytes())
        .await?;
    writer.write_all(b"\n").await?;

    let mut line = String::new();
    reader.read_line(&mut line).await?;
    if line.is_empty() {
        anyhow::bail!("daemon closed the connection without replying");
    }
    Ok(serde_json::from_str(&line)?)
}

fn render(response: Response, view: ListView, format: Format, quiet: bool) -> Result<i32> {
    match response {
        Response::Error { kind, message } => {
            eprintln!("error: {message}");
            if format == Format::Json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "ok": false,
                        "error": { "kind": kind, "message": message },
                    }))?
                );
            }
            Ok(kind.exit_code())
        }

        // A value: it is the result, so stdout carries it bare.
        Response::Ticket(id) => {
            emit_value(format, "id", &id)?;
            Ok(0)
        }
        Response::Token(token) => {
            emit_value(format, "token", &token)?;
            Ok(0)
        }

        // An action: nothing belongs on stdout in text mode, so the summary
        // goes to stderr where it cannot corrupt a pipe.
        Response::Done { dry_run, detail } => {
            match format {
                Format::Text => {
                    if !quiet {
                        let prefix = if dry_run { "would: " } else { "" };
                        eprintln!("{prefix}{detail}");
                    }
                }
                Format::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "ok": true,
                        "dry_run": dry_run,
                        "detail": detail,
                    }))?
                ),
            }
            Ok(0)
        }

        Response::List(info) => {
            match format {
                Format::Json => {
                    println!("{}", serde_json::to_string_pretty(&json_view(&info, view))?)
                }
                Format::Text => print!("{}", text_view(&info, view)),
            }
            Ok(0)
        }
    }
}

fn emit_value(format: Format, key: &str, value: &str) -> Result<()> {
    match format {
        Format::Text => println!("{value}"),
        Format::Json => println!("{}", serde_json::to_string_pretty(&json!({ key: value }))?),
    }
    Ok(())
}

fn json_view(info: &ListInfo, view: ListView) -> serde_json::Value {
    match view {
        ListView::Peers => json!({ "peers": info.peers }),
        ListView::Ports => json!({ "exposed": info.i_expose, "grants": info.grants }),
        _ => json!({
            "id": info.me,
            "peers": info.peers,
            "exposed": info.i_expose,
            "grants": info.grants,
            "bindings": info.bindings,
        }),
    }
}

fn text_view(info: &ListInfo, view: ListView) -> String {
    let mut out = String::new();

    if view == ListView::All {
        out.push_str(&format!("id  {}\n", info.me));
    }

    if matches!(view, ListView::All | ListView::Peers) {
        out.push_str("\npeers\n");
        if info.peers.is_empty() {
            out.push_str("  (none)\n");
        }
        for p in &info.peers {
            let ports = if p.they_expose.is_empty() {
                "-".to_string()
            } else {
                p.they_expose
                    .iter()
                    .map(u16::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            };
            out.push_str(&format!(
                "  {}  {:<10}  {:<7}  exposes {}\n",
                short(&p.key),
                p.label.as_deref().unwrap_or("-"),
                if p.online { "online" } else { "offline" },
                ports
            ));
        }
    }

    if matches!(view, ListView::All | ListView::Ports) {
        out.push_str("\nexposed\n");
        if info.i_expose.is_empty() {
            out.push_str("  (none)\n");
        }
        for e in &info.i_expose {
            let to: Vec<String> = info
                .grants
                .iter()
                .filter(|g| g.port == e.port)
                .map(|g| short(&g.to))
                .collect();
            out.push_str(&format!(
                "  {:<6}  {:<30}  to {}\n",
                e.port,
                e.backend,
                if to.is_empty() {
                    "(nobody)".to_string()
                } else {
                    to.join(",")
                }
            ));
        }
    }

    if view == ListView::All {
        out.push_str("\nbindings\n");
        if info.bindings.is_empty() {
            out.push_str("  (none)\n");
        }
        for b in &info.bindings {
            let landed = if b.local == b.port {
                format!("127.0.0.1:{}", b.local)
            } else {
                format!("127.0.0.1:{} (remapped from {})", b.local, b.port)
            };
            out.push_str(&format!("  {:<38}  from {}\n", landed, short(&b.peer)));
        }
    }

    out
}

/// Endpoint ids are 64 hex characters; a prefix is enough to tell peers apart
/// in a table, and the full value is always available from `--format json`.
fn short(key: &str) -> String {
    key.chars().take(12).collect()
}
