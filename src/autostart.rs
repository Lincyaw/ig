//! `ig autostart` -- hand the daemon to the platform's supervisor.
//!
//! `ig daemon` stays in the foreground on purpose. systemd and launchd both
//! want to own the process they started, and a tool that forks away leaves them
//! guessing at a pidfile, so self-daemonizing would buy backgrounding at the
//! cost of the thing that actually restarts you after a crash or a reboot.
//!
//! What was missing was never the fork. It was the boilerplate: knowing where
//! the unit file goes, what it has to say, and which two commands load it. That
//! is all this does -- write the unit, point it at this exact binary, and get
//! out of the way. `systemctl --user` and `launchctl` keep working normally
//! afterwards, and the unit is a plain file you can read and edit.

use crate::protocol::{ErrorKind, Failure};
use crate::Format;
use anyhow::{Context, Result};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;

/// What the supervisor is asked to run, resolved once so the unit records an
/// absolute path. A unit pointing at whatever `ig` is on `$PATH` at boot is a
/// unit that silently starts a different build than the one you installed.
struct Plan {
    exe: PathBuf,
    socket: PathBuf,
    args: Vec<String>,
}

impl Plan {
    fn new(socket: &Path, args: Vec<String>) -> Result<Self> {
        let exe = std::env::current_exe().context("cannot locate this binary")?;
        Ok(Self {
            exe,
            socket: socket.to_path_buf(),
            args,
        })
    }

    /// The full argument vector, `--socket` included even when it is the
    /// default: a unit that spells out what it runs stays correct if the
    /// default ever moves, and reads honestly when you open it a year later.
    fn argv(&self) -> Vec<String> {
        let mut argv = vec![
            self.exe.display().to_string(),
            "--socket".into(),
            self.socket.display().to_string(),
            "daemon".into(),
        ];
        argv.extend(self.args.iter().cloned());
        argv
    }
}

// ============================================================================
// Commands
// ============================================================================

pub fn install(socket: &Path, args: Vec<String>, dry_run: bool, format: Format) -> Result<i32> {
    let plan = Plan::new(socket, args)?;
    let path = platform::unit_path()?;
    let unit = platform::render(&plan);

    if dry_run {
        return done(
            format,
            true,
            &format!("write {} and load it", path.display()),
            Some(&unit),
        );
    }

    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create {}", parent.display()))?;
    std::fs::write(&path, &unit).with_context(|| format!("failed to write {}", path.display()))?;

    platform::load(&path)?;

    done(
        format,
        false,
        &format!("installed {} and started it", path.display()),
        None,
    )
}

pub fn uninstall(dry_run: bool, format: Format) -> Result<i32> {
    let path = platform::unit_path()?;
    if !path.exists() {
        return Err(Failure::new(
            ErrorKind::NotFound,
            format!("nothing installed at {}", path.display()),
        )
        .into());
    }

    if dry_run {
        return done(
            format,
            true,
            &format!("stop it and remove {}", path.display()),
            None,
        );
    }

    // Unload before unlinking: taking the file away first leaves the supervisor
    // holding a job it can no longer describe.
    platform::unload(&path)?;
    std::fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;

    done(format, false, &format!("removed {}", path.display()), None)
}

pub fn restart(dry_run: bool, format: Format) -> Result<i32> {
    let path = platform::unit_path()?;
    if !path.exists() {
        return Err(Failure::new(
            ErrorKind::NotFound,
            format!(
                "nothing installed at {}; run `ig autostart install`",
                path.display()
            ),
        )
        .into());
    }
    if dry_run {
        return done(format, true, "restart the supervised daemon", None);
    }
    platform::restart(&path)?;
    done(format, false, "restarted", None)
}

/// Reports where the unit is and whether it is running. A value, so it goes to
/// stdout in both formats.
pub fn status(format: Format) -> Result<i32> {
    let path = platform::unit_path()?;
    let installed = path.exists();
    let running = installed && platform::is_running();

    match format {
        Format::Json => println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "supervisor": platform::NAME,
                "unit": path.display().to_string(),
                "installed": installed,
                "running": running,
            }))?
        ),
        Format::Text => {
            println!("supervisor  {}", platform::NAME);
            println!("unit        {}", path.display());
            println!(
                "state       {}",
                match (installed, running) {
                    (false, _) => "not installed",
                    (true, false) => "installed, not running",
                    (true, true) => "running",
                }
            );
        }
    }
    Ok(0)
}

/// An action: nothing on stdout in text mode, so `ig autostart install` stays
/// safe to run inside a pipeline. `unit` is the rendered file, shown only for a
/// dry run, where seeing what would be written is the entire point.
fn done(format: Format, dry_run: bool, detail: &str, unit: Option<&str>) -> Result<i32> {
    match format {
        Format::Text => {
            let prefix = if dry_run { "would: " } else { "" };
            eprintln!("{prefix}{detail}");
            if let Some(unit) = unit {
                eprintln!();
                eprint!("{unit}");
            }
        }
        Format::Json => {
            let mut out = json!({ "ok": true, "dry_run": dry_run, "detail": detail });
            if let Some(unit) = unit {
                out["unit"] = json!(unit);
            }
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
    }
    Ok(0)
}

/// Run a supervisor command, turning a non-zero exit into its stderr rather
/// than a bare status code -- `launchctl` in particular says useful things
/// there and nowhere else.
fn run(program: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(program).args(args).output().map_err(|e| {
        Failure::new(
            ErrorKind::Unavailable,
            format!("cannot run {program}: {e}; is it installed?"),
        )
    })?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim();
        anyhow::bail!(
            "{program} {} failed: {}",
            args.join(" "),
            if err.is_empty() { "no output" } else { err }
        );
    }
    Ok(())
}

// ============================================================================
// systemd
// ============================================================================

#[cfg(target_os = "linux")]
mod platform {
    use super::{run, Plan};
    use anyhow::{Context, Result};
    use std::path::{Path, PathBuf};

    pub const NAME: &str = "systemd (user)";
    const UNIT: &str = "ig.service";

    pub fn unit_path() -> Result<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .context("neither XDG_CONFIG_HOME nor HOME is set")?;
        Ok(base.join("systemd/user").join(UNIT))
    }

    pub fn render(plan: &Plan) -> String {
        let exec = plan
            .argv()
            .iter()
            .map(|a| quote(a))
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            "# Generated by `ig autostart install`. Edit freely; `install` again\n\
             # to regenerate from the current arguments.\n\
             [Unit]\n\
             Description=ig\n\
             Documentation=https://github.com/Lincyaw/ig\n\
             After=network-online.target\n\
             Wants=network-online.target\n\
             \n\
             [Service]\n\
             # Not Type=forking: ig stays in the foreground so systemd tracks\n\
             # the process it started rather than guessing at a pidfile.\n\
             Type=simple\n\
             ExecStart={exec}\n\
             Restart=always\n\
             RestartSec=5\n\
             # The journal rotates. A redirected file does not.\n\
             StandardOutput=journal\n\
             StandardError=journal\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n"
        )
    }

    /// systemd splits ExecStart on whitespace and reads its own quoting, so any
    /// argument that is not plainly a word gets double-quoted.
    fn quote(arg: &str) -> String {
        if !arg.is_empty()
            && arg
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./:=,@+".contains(c))
        {
            return arg.to_string();
        }
        format!("\"{}\"", arg.replace('\\', "\\\\").replace('"', "\\\""))
    }

    /// The install sequence, as data so a test can hold it to the rule below.
    ///
    /// `restart` rather than `enable --now`: --now does nothing to a unit that
    /// is already running, so installing over one would rewrite the file and
    /// go on running the arguments you just replaced -- a config change that
    /// reports success and has no effect. The launchd path unloads first for
    /// exactly the same reason.
    pub fn load_plan() -> Vec<Vec<&'static str>> {
        vec![
            vec!["--user", "daemon-reload"],
            vec!["--user", "enable", UNIT],
            vec!["--user", "restart", UNIT],
        ]
    }

    pub fn load(_path: &Path) -> Result<()> {
        for args in load_plan() {
            run("systemctl", &args)?;
        }
        Ok(())
    }

    pub fn unload(_path: &Path) -> Result<()> {
        run("systemctl", &["--user", "disable", "--now", UNIT])
    }

    pub fn restart(_path: &Path) -> Result<()> {
        run("systemctl", &["--user", "restart", UNIT])
    }

    pub fn is_running() -> bool {
        std::process::Command::new("systemctl")
            .args(["--user", "is-active", "--quiet", UNIT])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

// ============================================================================
// launchd
// ============================================================================

#[cfg(target_os = "macos")]
mod platform {
    use super::{run, Plan};
    use anyhow::{Context, Result};
    use std::path::{Path, PathBuf};

    pub const NAME: &str = "launchd (LaunchAgent)";
    const LABEL: &str = "com.github.lincyaw.ig";

    pub fn unit_path() -> Result<PathBuf> {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(home)
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist")))
    }

    pub fn render(plan: &Plan) -> String {
        // A LaunchAgent, not a LaunchDaemon: ig runs as you, reads your key out
        // of your home directory, and should reach what your login session can.
        let args = plan
            .argv()
            .iter()
            .map(|a| format!("    <string>{}</string>\n", escape(a)))
            .collect::<String>();
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!-- Generated by `ig autostart install`. Edit freely; `install`\n\
             \x20    again to regenerate from the current arguments. -->\n\
             <plist version=\"1.0\">\n\
             <dict>\n\
             \x20 <key>Label</key>\n\
             \x20 <string>{LABEL}</string>\n\
             \x20 <key>ProgramArguments</key>\n\
             \x20 <array>\n\
             {args}\
             \x20 </array>\n\
             \x20 <key>RunAtLoad</key>\n\
             \x20 <true/>\n\
             \x20 <key>KeepAlive</key>\n\
             \x20 <true/>\n\
             \x20 <!-- launchd does not rotate these. Point them somewhere\n\
             \x20      newsyslog manages if the size matters. -->\n\
             \x20 <key>StandardOutPath</key>\n\
             \x20 <string>/tmp/ig.log</string>\n\
             \x20 <key>StandardErrorPath</key>\n\
             \x20 <string>/tmp/ig.log</string>\n\
             </dict>\n\
             </plist>\n"
        )
    }

    fn escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    }

    pub fn load(path: &Path) -> Result<()> {
        // Unload first so `install` over an existing agent is a replace rather
        // than an "already loaded" error.
        let _ = run("launchctl", &["unload", &path.display().to_string()]);
        run("launchctl", &["load", "-w", &path.display().to_string()])
    }

    pub fn unload(path: &Path) -> Result<()> {
        run("launchctl", &["unload", "-w", &path.display().to_string()])
    }

    pub fn restart(path: &Path) -> Result<()> {
        let _ = run("launchctl", &["unload", &path.display().to_string()]);
        run("launchctl", &["load", &path.display().to_string()])
    }

    pub fn is_running() -> bool {
        std::process::Command::new("launchctl")
            .args(["list", LABEL])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> Plan {
        Plan {
            exe: PathBuf::from("/usr/local/bin/ig"),
            socket: PathBuf::from("/tmp/ig.sock"),
            args: vec!["--service".into(), "/home/me/my services.toml".into()],
        }
    }

    #[test]
    fn the_unit_names_the_binary_by_absolute_path() {
        let argv = plan().argv();
        assert_eq!(argv[0], "/usr/local/bin/ig");
        assert!(argv.contains(&"daemon".to_string()));
        // Always spelled out, so the unit stays correct if the default moves.
        assert!(argv.contains(&"--socket".to_string()));
    }

    /// Installing over a running daemon has to replace it. The bug this
    /// guards against reported success, rewrote the unit, and left the old
    /// arguments running -- which is worse than failing, because you go on to
    /// debug the config you are not running.
    #[test]
    #[cfg(target_os = "linux")]
    fn installing_restarts_rather_than_only_enabling() {
        let plan = platform::load_plan();
        assert!(
            plan.iter().any(|args| args.contains(&"restart")),
            "install would be a no-op against a running unit: {plan:?}"
        );
    }

    #[test]
    fn an_argument_with_a_space_survives_the_unit() {
        let unit = platform::render(&plan());
        // Either quoted (systemd) or its own element (launchd), but never split
        // into two arguments the daemon would reject.
        assert!(
            unit.contains("\"/home/me/my services.toml\"")
                || unit.contains("<string>/home/me/my services.toml</string>"),
            "argument was not kept whole:\n{unit}"
        );
    }
}
