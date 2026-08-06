//! What a running daemon was told, kept across restarts.
//!
//! Grants, the backend behind a port, and where a peer's port lands here were
//! all memory only. A restart dropped every one of them without saying so,
//! which is the worst shape a bug can take: `ig status` showed the port a
//! moment earlier, and nothing reports that it is gone until something fails to
//! connect. The workaround was to re-declare everything from the unit file,
//! which quietly demotes a live control plane back into a config file.
//!
//! So the control socket writes what it is told. Only the control socket does:
//! declarations passed to `ig daemon` are an overlay for that run, applied on
//! top after this file is restored. Otherwise removing `-e 22` from a unit
//! would leave the port exposed forever, because an earlier boot had recorded
//! it -- a config file you cannot edit your way out of.

use crate::service::BackendSpec;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Bumped when a field changes meaning. A daemon that meets a newer file stops
/// rather than guessing at it and writing the guess back.
const VERSION: u32 = 1;

#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct State {
    #[serde(default)]
    pub version: u32,
    /// `(port, grantee)`, flat rather than keyed by port: one port can be
    /// granted to several peers, and a map would bury that.
    #[serde(default)]
    pub grants: Vec<(u16, String)>,
    /// What each exposed port is backed by, when something was declared.
    #[serde(default)]
    pub services: BTreeMap<u16, BackendSpec>,
    /// Announced port -> the local port it is remapped onto.
    #[serde(default)]
    pub binds: BTreeMap<u16, u16>,
}

impl State {
    pub fn is_empty(&self) -> bool {
        self.grants.is_empty() && self.services.is_empty() && self.binds.is_empty()
    }
}

pub struct Store {
    path: PathBuf,
}

impl Store {
    /// Beside the key, like the pin file. Everything one identity owns sits in
    /// one directory, so pointing `--key` elsewhere carries its state along
    /// rather than silently sharing the last daemon's.
    pub fn new(key_path: &Path) -> Self {
        Self {
            path: PathBuf::from(format!("{}.state.json", key_path.display())),
        }
    }

    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<State> {
        if !self.path.exists() {
            return Ok(State::default());
        }
        let data = std::fs::read(&self.path)
            .with_context(|| format!("failed to read {}", self.path.display()))?;
        let state: State = serde_json::from_slice(&data)
            .with_context(|| format!("{} is not readable state", self.path.display()))?;
        if state.version > VERSION {
            anyhow::bail!(
                "{} was written by a newer ig (state version {}, this build understands {VERSION})",
                self.path.display(),
                state.version
            );
        }
        Ok(state)
    }

    /// The version is stamped here rather than by every caller, so a snapshot
    /// built elsewhere cannot forget it and write a file that reads as v0.
    pub fn save(&self, state: &State) -> Result<()> {
        write_json(
            &self.path,
            &State {
                version: VERSION,
                grants: state.grants.clone(),
                services: state.services.clone(),
                binds: state.binds.clone(),
            },
        )
    }
}

/// Write JSON where an interrupted write cannot destroy what was there: a full
/// write to a sibling, then a rename, which is atomic within a filesystem.
///
/// 0600 throughout. A grant is an access decision and a pin is an
/// authorization; neither belongs to whatever the umask happens to be.
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
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

    let data = serde_json::to_vec_pretty(value)?;
    let tmp = PathBuf::from(format!("{}.{}.tmp", path.display(), std::process::id()));

    write_private(&tmp, &data).with_context(|| format!("failed to write {}", tmp.display()))?;

    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("failed to replace {}", path.display()));
    }
    Ok(())
}

fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    file.write_all(data)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempdir::Dir, Store) {
        let dir = tempdir::Dir::new();
        let store = Store::new(&dir.path().join("key"));
        (dir, store)
    }

    #[test]
    fn a_missing_file_is_empty_state_not_an_error() {
        let (_dir, store) = store();
        assert!(store.load().unwrap().is_empty());
    }

    #[test]
    fn what_was_saved_comes_back() {
        let (_dir, store) = store();
        let mut state = State {
            grants: vec![(22, "abc".into()), (22, "def".into())],
            ..State::default()
        };
        state.binds.insert(22, 2222);
        state.services.insert(
            5432,
            BackendSpec::Tcp {
                upstream: "db:5432".into(),
            },
        );

        store.save(&state).unwrap();
        let back = store.load().unwrap();

        assert_eq!(back.grants, state.grants);
        assert_eq!(back.binds, state.binds);
        assert_eq!(back.services.len(), 1);
        // Stamped on save, so an older build meeting this file can tell.
        assert_eq!(back.version, VERSION);
    }

    #[test]
    fn the_file_is_not_readable_by_anyone_else() {
        let (_dir, store) = store();
        store.save(&State::default()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "grants decide access; 0600 or nothing");
        }
    }

    #[test]
    fn a_newer_file_stops_the_daemon_rather_than_being_guessed_at() {
        let (_dir, store) = store();
        std::fs::write(store.path(), br#"{"version": 999}"#).unwrap();
        let err = store.load().unwrap_err().to_string();
        assert!(err.contains("newer ig"), "unhelpful: {err}");
    }

    #[test]
    fn a_save_that_lands_leaves_no_temporary_behind() {
        let (dir, store) = store();
        store.save(&State::default()).unwrap();
        let strays: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(strays.is_empty(), "left {} temp file(s)", strays.len());
    }

    /// A directory that removes itself, so these tests do not depend on a crate
    /// the binary would otherwise never need.
    mod tempdir {
        use std::path::{Path, PathBuf};
        pub struct Dir(PathBuf);
        impl Dir {
            pub fn new() -> Self {
                use std::sync::atomic::{AtomicU32, Ordering};
                static NEXT: AtomicU32 = AtomicU32::new(0);
                let path = std::env::temp_dir().join(format!(
                    "ig-state-test-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&path).unwrap();
                Dir(path)
            }
            #[cfg(test)]
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
