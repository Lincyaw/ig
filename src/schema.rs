//! `--dump-schema`: the whole command tree as JSON.
//!
//! An agent driving this CLI cannot ask a human what a flag accepts. Walking
//! clap's own definition means the schema cannot drift from the parser -- it is
//! generated from the thing that does the parsing, not maintained beside it.
//!
//! Diff this between releases: a removed flag, a narrowed type, or a dropped
//! enum value is a breaking change to the contract.

use anyhow::Result;
use clap::{Arg, Command};
use serde_json::{json, Value};

pub fn dump(cmd: &Command) -> Result<String> {
    let root = json!({
        "schema_version": 1,
        "name": cmd.get_name(),
        "version": cmd.get_version().unwrap_or("unknown"),
        "exit_codes": exit_codes(),
        "command": describe(cmd),
    });
    Ok(serde_json::to_string_pretty(&root)?)
}

/// The portable half of the exit code table, emitted so a caller can map codes
/// without reading the docs.
fn exit_codes() -> Value {
    json!({
        "0": "success",
        "1": "internal error",
        "2": "invalid arguments or request",
        "3": "not found",
        "4": "denied",
        "5": "conflict",
        "7": "daemon unavailable",
    })
}

fn describe(cmd: &Command) -> Value {
    let args: Vec<Value> = cmd
        .get_arguments()
        .filter(|a| !matches!(a.get_id().as_str(), "help" | "version"))
        .map(describe_arg)
        .collect();

    let subcommands: Vec<Value> = cmd.get_subcommands().map(describe).collect();

    let mut out = json!({
        "name": cmd.get_name(),
        "about": cmd.get_about().map(|s| s.to_string()),
        "args": args,
    });
    if !subcommands.is_empty() {
        out["subcommands"] = json!(subcommands);
    }
    out
}

fn describe_arg(arg: &Arg) -> Value {
    let values: Vec<String> = arg
        .get_possible_values()
        .iter()
        .map(|v| v.get_name().to_string())
        .collect();

    let mut out = json!({
        "name": arg.get_id().as_str(),
        "positional": arg.is_positional(),
        "required": arg.is_required_set(),
        "repeatable": matches!(
            arg.get_action(),
            clap::ArgAction::Append | clap::ArgAction::Count
        ),
        "takes_value": arg.get_num_args().map(|n| n.takes_values()).unwrap_or(false),
        "help": arg.get_help().map(|s| s.to_string()),
    });

    if let Some(long) = arg.get_long() {
        out["long"] = json!(format!("--{long}"));
    }
    if let Some(short) = arg.get_short() {
        out["short"] = json!(format!("-{short}"));
    }
    if let Some(name) = arg.get_value_names().and_then(|n| n.first()) {
        out["value_name"] = json!(name.to_string());
    }
    if !values.is_empty() {
        out["values"] = json!(values);
    }
    if let Some(default) = arg.get_default_values().first() {
        out["default"] = json!(default.to_string_lossy());
    }
    if let Some(env) = arg.get_env() {
        out["env"] = json!(env.to_string_lossy());
    }

    out
}
