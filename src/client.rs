//! CLI client - sends commands to daemon over Unix socket.

use crate::protocol::{Request, Response};
use crate::service::{self, BackendSpec};
use crate::Command;
use anyhow::{Context, Result};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub async fn send_command(socket_path: &Path, command: Command) -> Result<()> {
    let request = match command {
        Command::AddPeer { ticket } => Request::AddPeer { ticket },
        Command::RemovePeer { ticket } => Request::RemovePeer { ticket },
        Command::Expose {
            port,
            to,
            upstream,
            unix,
            routes,
        } => {
            // Parse the route table here rather than in the daemon so a bad
            // table is reported against the file the user just pointed at.
            let backend = match (upstream, unix, routes) {
                (Some(upstream), None, None) => Some(BackendSpec::Tcp { upstream }),
                (None, Some(path), None) => Some(BackendSpec::Unix { path }),
                (None, None, Some(file)) => Some(BackendSpec::Http {
                    routes: service::load_routes(&file)?,
                }),
                _ => None,
            };
            Request::Expose { port, to, backend }
        }
        // clap guarantees exactly one of --local / --clear is present.
        Command::Bind { port, local, clear } => Request::Bind {
            port,
            local: if clear { None } else { local },
        },
        Command::Unexpose { port, to } => Request::Unexpose { port, to },
        Command::List => Request::List,
        Command::Ticket => Request::Ticket,
        Command::GrantToken { label } => Request::GrantToken { label },
        Command::Pin { key, label } => Request::Pin { key, label },
        Command::Daemon { .. } => unreachable!("daemon handled separately"),
    };

    let stream = UnixStream::connect(socket_path)
        .await
        .context("failed to connect to daemon - is it running?")?;

    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // Send request
    let request_json = serde_json::to_string(&request)?;
    writer.write_all(request_json.as_bytes()).await?;
    writer.write_all(b"\n").await?;

    // Read response
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let response: Response = serde_json::from_str(&line)?;

    // Print response
    match response {
        Response::Ok => println!("OK"),
        Response::Ticket(ticket) => println!("{}", ticket),
        Response::Token(token) => println!("{}", token),
        Response::List(info) => {
            println!("{}", serde_json::to_string_pretty(&info)?);
        }
        Response::Error(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }

    Ok(())
}
