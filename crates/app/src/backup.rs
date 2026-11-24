//! Options → Backup: the whole MDM state in one file, to move it to a new Windows (the
//! user's ask). It holds what `downloads.json` holds (the list, the settings, the queues,
//! the columns) and every unfinished download's part file and resume state, with their
//! original paths, so a restore puts everything back as it was.
//!
//! The file is JSON with a magic header: readable (and recoverable) by hand, with the part
//! files' bytes base64 inside. Passwords are not in it: they live in the user's keychain,
//! which never travels in a file MDM writes.

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::list::Layout;
use crate::model::Item;
use crate::queue::Queue;
use crate::settings::Settings;

/// A restored backup says what it wants back, not that it is the truth: the list reloads
/// from it, the files go back beside their destinations.
#[derive(Serialize, Deserialize)]
pub struct Backup {
    /// `MDM-BACKUP`, and its version, so a future format can be told apart.
    magic: String,
    version: u32,
    when: u64,
    next_id: u64,
    /// The download list as it is stored (borrowed from the window's, not cloned).
    items: serde_json::Value,
    #[serde(default)]
    columns: Option<Layout>,
    #[serde(default)]
    settings: Settings,
    #[serde(default)]
    queues: Vec<Queue>,
    /// Unfinished downloads: destination, part file's bytes, state file's bytes (both
    /// base64, both optional — a download without a part file yet has neither).
    #[serde(default)]
    files: Vec<BackedFile>,
}

const MAGIC: &str = "MDM-BACKUP";
/// 1: the format above.
const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct BackedFile {
    dest: PathBuf,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    part: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    state: String,
}

/// Everything MDM has, as one value to write.
pub struct Taken {
    pub backup: Backup,
    /// Files that could not be read (deleted while backing up): named in the summary.
    pub skipped: Vec<String>,
}

/// Take a backup of the app's state: the list file's contents (already loaded in the window)
/// and every unfinished download's part file and resume state, read off the disk.
pub fn take(
    next_id: u64,
    items: &[Item],
    columns: Option<&Layout>,
    settings: &Settings,
    queues: &[Queue],
) -> Result<Taken, String> {
    let mut files = Vec::new();
    let mut skipped = Vec::new();
    for item in items {
        if matches!(
            item.status,
            crate::model::Status::Complete | crate::model::Status::Error
        ) {
            continue;
        }
        let dest = item.dest.clone();
        let mut backed = BackedFile {
            dest: dest.clone(),
            part: String::new(),
            state: String::new(),
        };
        for (field, path) in [
            (&mut backed.part, mdm_engine::naming::part_path(&dest)),
            (&mut backed.state, mdm_engine::state::state_path(&dest)),
        ] {
            match std::fs::File::open(&path).and_then(|mut f| {
                let mut bytes = Vec::new();
                f.read_to_end(&mut bytes)?;
                Ok(bytes)
            }) {
                Ok(bytes) => *field = base64::engine::general_purpose::STANDARD.encode(&bytes),
                Err(_) if !path.exists() => {}
                Err(e) => skipped.push(format!("{} ({e})", path.display())),
            }
        }
        if !backed.part.is_empty() || !backed.state.is_empty() {
            files.push(backed);
        }
    }
    Ok(Taken {
        backup: Backup {
            magic: MAGIC.to_owned(),
            version: VERSION,
            when: now(),
            next_id,
            items: serde_json::to_value(items).map_err(|e| e.to_string())?,
            columns: columns.cloned(),
            settings: settings.clone(),
            queues: queues.to_vec(),
            files,
        },
        skipped,
    })
}

impl Backup {
    /// Read one back from disk; anything but this format's own header fails it.
    pub fn load(p: &Path) -> Result<Backup, String> {
        let bytes = std::fs::read(p).map_err(|e| e.to_string())?;
        let backup: Backup =
            serde_json::from_slice(&bytes).map_err(|e| format!("not an MDM backup: {e}"))?;
        if backup.magic != MAGIC || backup.version != VERSION {
            return Err(format!(
                "a {} version {} file, not this version's",
                backup.magic, backup.version
            ));
        }
        Ok(backup)
    }

    /// A short line for the file list: what and when (the machine's local date).
    pub fn summary(&self) -> String {
        // i18n: Options → Backup: the summary of the backup just taken, "{n}" the number of
        // downloads in it
        crate::i18n::fmt(
            crate::i18n::t("{n} downloads, {d} unfinished files"),
            &[
                ("n", &self.items.as_array().map_or(0, Vec::len).to_string()),
                ("d", &self.files.len().to_string()),
            ],
        )
        .to_string()
    }

    /// Restore it: write the list as the app's own, and every unfinished file's part file and
    /// resume state back beside its destination (the folder can have moved: its parent is
    /// created when it is gone). Returns (restored, skipped) names for the summary line.
    pub fn restore(self) -> Result<(usize, Vec<String>), String> {
        // The list first: if this fails (no data dir), nothing else is touched.
        let p = crate::store::path().ok_or("no data folder")?;
        write_list(&p, &self)?;
        let mut restored = 0;
        let mut skipped = Vec::new();
        for f in self.files {
            let mut done = 0;
            for (text, path) in [
                (&f.part, mdm_engine::naming::part_path(&f.dest)),
                (&f.state, mdm_engine::state::state_path(&f.dest)),
            ] {
                if text.is_empty() {
                    continue;
                }
                let bytes = match base64::engine::general_purpose::STANDARD.decode(text) {
                    Ok(b) => b,
                    Err(e) => {
                        skipped.push(format!("{} ({e})", path.display()));
                        continue;
                    }
                };
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                match std::fs::write(&path, &bytes) {
                    Ok(()) => done += 1,
                    Err(e) => skipped.push(format!("{} ({e})", path.display())),
                }
            }
            if done > 0 {
                restored += 1;
            }
        }
        Ok((restored, skipped))
    }
}

/// The list file's own shape, with this backup's contents (store.rs's `ListRef`: version,
/// next_id, items, columns, settings, queues — read back by the app at its next start, or
/// when the window reloads it).
fn write_list(p: &Path, backup: &Backup) -> Result<(), String> {
    #[derive(Serialize)]
    struct ListFile<'a> {
        version: u32,
        next_id: u64,
        items: &'a serde_json::Value,
        columns: Option<&'a Layout>,
        settings: &'a Settings,
        queues: &'a [Queue],
    }
    let text = serde_json::to_vec_pretty(&ListFile {
        version: 2,
        next_id: backup.next_id,
        items: &backup.items,
        columns: backup.columns.as_ref(),
        settings: &backup.settings,
        queues: &backup.queues,
    })
    .map_err(|e| e.to_string())?;
    // A restore over a list that is there keeps the old one beside it, as its generations do.
    if p.exists() {
        let _ = std::fs::copy(p, p.with_extension("json.pre-restore.bak"));
    }
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(p, text).map_err(|e| e.to_string())
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
