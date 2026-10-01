//! In-progress marker of the OFFLINE rewrite tools (`compress-existing`,
//! `decompress-existing`, `encrypt-existing`, `decrypt-existing`).
//!
//! Those tools swap a blob and its sidecar with a journal
//! ([`arca_storage::inplace`]). A crash between the two swaps leaves a
//! mismatched pair that only a re-run of a rewrite tool repairs. The server
//! must never start on such a pair (ciphertext would be served as plaintext),
//! so a real (non dry-run) run drops this marker durably BEFORE touching any
//! blob and removes it only when it finished cleanly. `arca serve` and the
//! other data-reading offline commands check for it.
//!
//! # Lifecycle
//!
//! * [`begin`]: writes the marker (atomic + fsync). Refused when a marker of
//!   ANOTHER command exists. The same command may run again whatever its
//!   filters: `resolve_pending` runs on every sidecar before the filters are
//!   applied, so any run of that command repairs everything that is pending.
//! * [`finish`]: removes the marker and fsyncs the directory. Called only when
//!   every candidate was processed without error; a failed run (or a crash)
//!   leaves the marker in place.
//! * [`ensure_none`]: the guard used by `serve`, `recover`, `gc`,
//!   `migrate-db` and `migrate-topology`; [`warn_if_present`] is the soft
//!   variant used by the read-only `fsck`.
//!
//! The marker file must never be deleted by hand: see the CLI guide.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use arca_storage::fs::{sync_dir, write_file_atomic};
use serde::{Deserialize, Serialize};

/// File name of the marker, directly inside the data directory.
pub const MARKER_FILE: &str = ".offline-rewrite-in-progress";

pub fn marker_path(data_dir: &Path) -> PathBuf {
    data_dir.join(MARKER_FILE)
}

/// Content of the marker file (small JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    /// Subcommand name, e.g. `compress-existing`.
    pub command: String,
    /// Arguments needed to re-run it (filters, algorithm), without `--config`
    /// and `--dry-run`.
    pub args: Vec<String>,
    /// RFC 3339 start time.
    pub started_at: String,
    pub pid: u32,
}

impl Marker {
    /// The command line the operator must run, without `--config`.
    pub fn rerun_command(&self) -> String {
        let mut s = format!("arca {}", self.command);
        for a in &self.args {
            s.push(' ');
            s.push_str(a);
        }
        s
    }
}

/// What is on disk at the marker path.
#[derive(Debug)]
pub enum MarkerState {
    Absent,
    Present(Marker),
    /// The file exists but cannot be parsed. Still treated as present.
    Unreadable(String),
}

pub fn read(data_dir: &Path) -> Result<MarkerState> {
    let path = marker_path(data_dir);
    match std::fs::read_to_string(&path) {
        Ok(json) => Ok(match serde_json::from_str::<Marker>(&json) {
            Ok(m) => MarkerState::Present(m),
            Err(e) => MarkerState::Unreadable(e.to_string()),
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(MarkerState::Absent),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Describes a pending marker for an error message.
fn describe(data_dir: &Path, state: &MarkerState) -> String {
    let path = marker_path(data_dir);
    let what = match state {
        MarkerState::Present(m) => format!(
            "An offline rewrite was started at {} (pid {}) and did not finish cleanly.\n\
             Blobs and sidecars may be mismatched until it is completed.\n\
             Re-run it with the same --config: {}",
            m.started_at,
            m.pid,
            m.rerun_command()
        ),
        MarkerState::Unreadable(e) => format!(
            "An offline rewrite did not finish cleanly and its marker is unreadable ({e}).\n\
             Re-run the offline rewrite command that was interrupted \
             (compress-existing, decompress-existing, encrypt-existing or decrypt-existing)."
        ),
        MarkerState::Absent => String::new(),
    };
    format!(
        "{what}\nMarker: {}\nDo NOT delete the marker by hand: it is removed automatically when the re-run finishes.",
        path.display()
    )
}

/// Writes the marker before a real rewrite run starts.
pub fn begin(data_dir: &Path, command: &str, args: Vec<String>) -> Result<()> {
    match read(data_dir)? {
        MarkerState::Absent => {}
        MarkerState::Present(m) if m.command == command => {}
        other => anyhow::bail!(
            "refusing to run {command}: another offline rewrite is unfinished.\n{}",
            describe(data_dir, &other)
        ),
    }
    let marker = Marker {
        command: command.to_string(),
        args,
        started_at: chrono::Utc::now().to_rfc3339(),
        pid: std::process::id(),
    };
    let json = serde_json::to_vec_pretty(&marker)?;
    let path = marker_path(data_dir);
    write_file_atomic(&path, &json).with_context(|| format!("writing {}", path.display()))
}

/// Removes the marker durably after a clean run. A missing marker is fine.
pub fn finish(data_dir: &Path) -> Result<()> {
    let path = marker_path(data_dir);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
    }
    sync_dir(data_dir).with_context(|| format!("syncing {}", data_dir.display()))
}

/// Fails when a marker exists. `action` is what was about to run.
pub fn ensure_none(data_dir: &Path, action: &str) -> Result<()> {
    match read(data_dir)? {
        MarkerState::Absent => Ok(()),
        state => anyhow::bail!("refusing to {action}.\n{}", describe(data_dir, &state)),
    }
}

/// Prints a warning to stderr when a marker exists (for read-only commands).
pub fn warn_if_present(data_dir: &Path) {
    match read(data_dir) {
        Ok(MarkerState::Absent) => {}
        Ok(state) => eprintln!("WARNING: results may be unreliable.\n{}", describe(data_dir, &state)),
        Err(e) => eprintln!("WARNING: cannot check the offline rewrite marker: {e:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> Vec<String> {
        vec!["--bucket".into(), "b".into()]
    }

    #[test]
    fn begin_writes_json_with_command_args_time_and_pid() {
        let tmp = tempfile::tempdir().unwrap();
        begin(tmp.path(), "compress-existing", args()).unwrap();
        let json = std::fs::read_to_string(marker_path(tmp.path())).unwrap();
        let m: Marker = serde_json::from_str(&json).unwrap();
        assert_eq!(m.command, "compress-existing");
        assert_eq!(m.args, args());
        assert_eq!(m.pid, std::process::id());
        assert!(chrono::DateTime::parse_from_rfc3339(&m.started_at).is_ok());
        assert_eq!(m.rerun_command(), "arca compress-existing --bucket b");
    }

    #[test]
    fn finish_removes_the_marker_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        begin(tmp.path(), "encrypt-existing", vec![]).unwrap();
        assert!(marker_path(tmp.path()).exists());
        finish(tmp.path()).unwrap();
        assert!(!marker_path(tmp.path()).exists());
        finish(tmp.path()).unwrap();
    }

    #[test]
    fn same_command_may_begin_again_with_other_filters() {
        let tmp = tempfile::tempdir().unwrap();
        begin(tmp.path(), "compress-existing", args()).unwrap();
        begin(tmp.path(), "compress-existing", vec![]).unwrap();
        let MarkerState::Present(m) = read(tmp.path()).unwrap() else { panic!("no marker") };
        assert!(m.args.is_empty());
    }

    #[test]
    fn different_command_is_refused_and_names_the_one_to_rerun() {
        let tmp = tempfile::tempdir().unwrap();
        begin(tmp.path(), "encrypt-existing", args()).unwrap();
        let err = begin(tmp.path(), "decompress-existing", vec![]).unwrap_err().to_string();
        assert!(err.contains("arca encrypt-existing --bucket b"), "{err}");
        assert!(err.contains(MARKER_FILE), "{err}");
        // The original marker is untouched.
        let MarkerState::Present(m) = read(tmp.path()).unwrap() else { panic!("no marker") };
        assert_eq!(m.command, "encrypt-existing");
    }

    #[test]
    fn ensure_none_passes_without_marker_and_refuses_with_one() {
        let tmp = tempfile::tempdir().unwrap();
        ensure_none(tmp.path(), "start the server").unwrap();
        begin(tmp.path(), "decrypt-existing", args()).unwrap();
        let err = ensure_none(tmp.path(), "start the server").unwrap_err().to_string();
        assert!(err.contains("refusing to start the server"), "{err}");
        assert!(err.contains("arca decrypt-existing --bucket b"), "{err}");
        assert!(err.contains(&marker_path(tmp.path()).display().to_string()), "{err}");
    }

    #[test]
    fn unreadable_marker_still_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(marker_path(tmp.path()), "not json").unwrap();
        assert!(ensure_none(tmp.path(), "start the server").is_err());
        assert!(begin(tmp.path(), "compress-existing", vec![]).is_err());
    }
}
