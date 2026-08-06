//! `ig upgrade` -- replace this binary with the latest release.
//!
//! Three things make this safe enough to run unattended. The asset is named
//! after the triple this binary was compiled for, taken from the compiler at
//! build time, so a binary can only ever fetch the artifact it already is. What
//! comes down is checked against the release's own `SHA256SUMS` before anything
//! on disk moves. And the swap is a rename within the same directory, which is
//! atomic: an interrupted upgrade leaves the old binary, never half a new one.
//!
//! Replacing a running executable is fine on Unix -- the kernel keeps the old
//! inode alive for processes already using it, so a supervised daemon keeps
//! running the old code until it is restarted, which is what we tell the user.

use crate::protocol::{ErrorKind, Failure};
use crate::Format;
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};

const REPO: &str = "Lincyaw/ig";
const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// The triple this binary was built for, recorded by build.rs.
const TARGET: &str = env!("IG_TARGET");
const SUMS: &str = "SHA256SUMS";

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    /// The API url, not `browser_download_url`: only this one accepts a token,
    /// and the repository is private.
    url: String,
}

impl Release {
    fn version(&self) -> &str {
        self.tag_name.trim_start_matches('v')
    }

    fn asset(&self, name: &str) -> Result<&Asset> {
        self.assets.iter().find(|a| a.name == name).ok_or_else(|| {
            Failure::new(
                ErrorKind::NotFound,
                format!("{} has no {name}", self.tag_name),
            )
            .into()
        })
    }
}

pub async fn run(dry_run: bool, format: Format) -> Result<i32> {
    let asset_name = format!("ig-{TARGET}");
    let token = token();
    let client = client()?;

    let release = latest(&client, token.as_deref()).await?;
    let latest_version = release.version().to_string();

    if latest_version == CURRENT {
        return report(
            format,
            dry_run,
            &latest_version,
            false,
            &format!("already on {CURRENT}"),
        );
    }

    if dry_run {
        return report(
            format,
            true,
            &latest_version,
            false,
            &format!("upgrade {CURRENT} -> {latest_version}"),
        );
    }

    let binary = fetch(&client, token.as_deref(), release.asset(&asset_name)?).await?;
    let sums = fetch(&client, token.as_deref(), release.asset(SUMS)?).await?;
    verify(&binary, &String::from_utf8_lossy(&sums), &asset_name)?;

    let path = replace(&binary)?;

    report(
        format,
        false,
        &latest_version,
        true,
        &format!(
            "upgraded {CURRENT} -> {latest_version} at {}; restart the daemon to run it",
            path.display()
        ),
    )
}

/// An action, so text mode writes nothing to stdout. `current` and `latest` are
/// carried in the JSON alongside the usual fields because a caller deciding
/// whether to roll this out needs the versions, not the prose.
fn report(
    format: Format,
    dry_run: bool,
    latest: &str,
    upgraded: bool,
    detail: &str,
) -> Result<i32> {
    match format {
        Format::Text => {
            let prefix = if dry_run { "would: " } else { "" };
            eprintln!("{prefix}{detail}");
        }
        Format::Json => println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "dry_run": dry_run,
                "current": CURRENT,
                "latest": latest,
                "upgraded": upgraded,
                "detail": detail,
            }))?
        ),
    }
    Ok(0)
}

// ============================================================================
// GitHub
// ============================================================================

/// A token from the environment, or whatever `gh` is already logged in with.
/// Falling back to `gh` means a machine where you have used `gh` needs no
/// setup, while a bare server only needs one exported variable.
fn token() -> Option<String> {
    for var in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(token) = std::env::var(var) {
            if !token.trim().is_empty() {
                return Some(token.trim().to_string());
            }
        }
    }
    let out = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let token = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!token.is_empty()).then_some(token)
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        // GitHub rejects requests without one.
        .user_agent(concat!("ig/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("failed to build an HTTP client")
}

async fn latest(client: &reqwest::Client, token: Option<&str>) -> Result<Release> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let body = get(client, token, &url, "application/vnd.github+json").await?;
    serde_json::from_slice(&body).context("could not read the release listing from GitHub")
}

async fn fetch(client: &reqwest::Client, token: Option<&str>, asset: &Asset) -> Result<Vec<u8>> {
    get(client, token, &asset.url, "application/octet-stream").await
}

async fn get(
    client: &reqwest::Client,
    token: Option<&str>,
    url: &str,
    accept: &str,
) -> Result<Vec<u8>> {
    let mut req = client.get(url).header("Accept", accept);
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }

    let response = req
        .send()
        .await
        .map_err(|e| Failure::new(ErrorKind::Unavailable, format!("cannot reach GitHub ({e})")))?;

    let status = response.status();
    if !status.is_success() {
        // A private repository with no credentials looks exactly like one that
        // does not exist, so say what to do about both.
        let kind = match status.as_u16() {
            401 | 403 => ErrorKind::Denied,
            404 => ErrorKind::NotFound,
            _ => ErrorKind::Unavailable,
        };
        let hint = if token.is_none() {
            "; set GH_TOKEN or run `gh auth login` -- the repository is private"
        } else {
            ""
        };
        return Err(Failure::new(kind, format!("GitHub answered {status}{hint}")).into());
    }

    Ok(response
        .bytes()
        .await
        .context("the download was cut short")?
        .to_vec())
}

// ============================================================================
// Verify and swap
// ============================================================================

/// Check the download against the release's own SHA256SUMS. Without this the
/// upgrade path is "run whatever the network handed us".
fn verify(binary: &[u8], sums: &str, name: &str) -> Result<()> {
    let digest = ring::digest::digest(&ring::digest::SHA256, binary);
    let got: String = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();

    let want = sums
        .lines()
        .filter_map(|line| line.split_once("  "))
        .find(|(_, file)| file.trim() == name)
        .map(|(sum, _)| sum.trim().to_lowercase())
        .ok_or_else(|| {
            Failure::new(ErrorKind::NotFound, format!("{SUMS} does not cover {name}"))
        })?;

    if got != want {
        return Err(Failure::new(
            ErrorKind::Invalid,
            format!("{name} failed its checksum: expected {want}, got {got}"),
        )
        .into());
    }
    Ok(())
}

/// Swap the new binary in atomically. The temporary file is a sibling so the
/// rename stays within one filesystem, which is what makes it atomic; a
/// temp directory elsewhere would silently degrade to copy-then-delete.
fn replace(binary: &[u8]) -> Result<PathBuf> {
    let exe = std::env::current_exe().context("cannot locate this binary")?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", exe.display()))?;

    let tmp = dir.join(format!(".ig-upgrade-{}", std::process::id()));
    write_executable(&tmp, binary).map_err(|e| {
        Failure::new(
            ErrorKind::Denied,
            format!(
                "cannot write to {}: {e}; install ig somewhere you own, or re-run with sudo",
                dir.display()
            ),
        )
    })?;

    if let Err(e) = std::fs::rename(&tmp, &exe) {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow::anyhow!("failed to replace {}: {e}", exe.display()));
    }
    Ok(exe)
}

fn write_executable(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o755);
    }
    let mut file = opts.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_asset_is_named_for_the_build() {
        // Nothing to compare against but the compiler, which is the point: the
        // release workflow names assets from the same triple.
        assert!(!TARGET.is_empty());
        assert!(format!("ig-{TARGET}").starts_with("ig-"));
    }

    #[test]
    fn a_matching_checksum_passes_and_a_wrong_one_does_not() {
        let sums = "\
e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  ig-other\n\
9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08  ig-mine\n";
        assert!(verify(b"test", sums, "ig-mine").is_ok());
        assert!(verify(b"tampered", sums, "ig-mine").is_err());
    }

    #[test]
    fn an_uncovered_asset_is_a_failure_not_a_pass() {
        // The dangerous bug here is treating "no line for me" as nothing to
        // check, which would let an unlisted asset install unverified.
        assert!(verify(b"anything", "", "ig-mine").is_err());
    }

    #[test]
    fn a_tag_is_compared_without_its_v() {
        let release = Release {
            tag_name: "v1.2.3".into(),
            assets: vec![],
        };
        assert_eq!(release.version(), "1.2.3");
    }
}
