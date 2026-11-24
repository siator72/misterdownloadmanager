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
use std::sync::Arc;

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

/// What the user asked the export or the import to cover (the Backup page's check boxes).
/// Files: the unfinished downloads' part and state files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct What {
    pub list: bool,
    pub settings: bool,
    pub files: bool,
}

impl What {
    pub const ALL: Self = Self {
        list: true,
        settings: true,
        files: true,
    };

    /// None of the three can be left out (an empty backup says nothing).
    pub fn any(self) -> bool {
        self.list || self.settings || self.files
    }
}

/// A backup (or a restore of one) running on a worker thread, for the window that
/// shows its progress (Options → Backup): the step it is on (how far, doing what) and
/// how it ended.
#[derive(Clone)]
pub struct Job {
    step: Arc<std::sync::Mutex<Step>>,
}

#[derive(Clone, PartialEq)]
pub enum Step {
    /// Not begun: the bar shows empty.
    Waiting,
    /// `done` of `total`, `what` says what (a file or a part of the state).
    Work {
        done: usize,
        total: usize,
        what: &'static str,
    },
    /// Finished; `Err` says what failed, `Ok` the skipped files' names (empty if none).
    Done(Result<String, String>),
}

impl Job {
    fn new() -> Self {
        Self {
            step: Arc::new(std::sync::Mutex::new(Step::Waiting)),
        }
    }

    /// What the window should draw now.
    pub fn step(&self) -> Step {
        self.step
            .lock()
            .map(|s| s.clone())
            .unwrap_or(Step::Done(Err("…".into())))
    }

    fn set(&self, step: Step) {
        if let Ok(mut s) = self.step.lock() {
            *s = step;
        }
    }
}

#[derive(Serialize, Deserialize)]
struct BackedFile {
    dest: PathBuf,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    part: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    state: String,
}

/// Everything MDM has, as one value to write.
#[allow(dead_code)] // the job path (snapshot + take_job) replaced it; kept for its tests
pub struct Taken {
    pub backup: Backup,
    /// Files that could not be read (deleted while backing up): named in the summary.
    pub skipped: Vec<String>,
}

/// Take a backup of the app's state: the list file's contents (already loaded in the window)
/// and every unfinished download's part file and resume state, read off the disk.
#[allow(dead_code)] // the job path (snapshot + take_job) replaced it; kept for its tests
pub fn take(
    next_id: u64,
    items: &[Item],
    columns: Option<&Layout>,
    settings: &Settings,
    queues: &[Queue],
    what: What,
    mut progress: impl FnMut(usize, usize),
) -> Result<Taken, String> {
    let unfinished: Vec<&Item> = items
        .iter()
        .filter(|i| {
            !matches!(
                i.status,
                crate::model::Status::Complete | crate::model::Status::Error
            )
        })
        .collect();
    let mut files = Vec::new();
    let mut skipped = Vec::new();
    for (nth, item) in unfinished.iter().enumerate() {
        progress(nth, unfinished.len());
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
    progress(unfinished.len(), unfinished.len());
    Ok(Taken {
        backup: Backup {
            magic: MAGIC.to_owned(),
            version: VERSION,
            when: now(),
            next_id: what.list.then_some(next_id).unwrap_or(0),
            items: if what.list {
                serde_json::to_value(items).map_err(|e| e.to_string())?
            } else {
                serde_json::Value::Array(Vec::new())
            },
            columns: what.list.then(|| columns.cloned()).flatten(),
            settings: if what.settings {
                settings.clone()
            } else {
                Settings::default()
            },
            queues: if what.list {
                queues.to_vec()
            } else {
                Vec::new()
            },
            files: if what.files { files } else { Vec::new() },
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
    #[allow(dead_code)] // the job path's window shows counts live instead
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
    /// created when it is gone). `what` says what the user asked back (the rest of the backup
    /// is left out), `progress` is called per file. Returns (restored, skipped) names.
    pub fn restore(
        mut self,
        what: What,
        mut progress: impl FnMut(usize, usize),
    ) -> Result<(usize, Vec<String>), String> {
        if !what.settings {
            self.settings = Settings::default();
        }
        if !what.list {
            self.items = serde_json::Value::Array(Vec::new());
            self.columns = None;
            self.queues = Vec::new();
        }
        if !what.files {
            self.files = Vec::new();
        }
        // The list first: if this fails (no data dir), nothing else is touched.
        let p = crate::store::path().ok_or("no data folder")?;
        write_list(&p, &self)?;
        let mut restored = 0;
        let mut skipped = Vec::new();
        let total = self.files.len();
        for (nth, f) in self.files.into_iter().enumerate() {
            progress(nth, total);
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

/// The state as the backup's JSON before its files are gathered (the window's thread
/// serializes it here: a running download's `live` does not clone across a thread).
pub fn snapshot(
    next_id: u64,
    items: &[Item],
    columns: Option<&Layout>,
    settings: &Settings,
    queues: &[Queue],
    what: What,
) -> serde_json::Value {
    serde_json::json!({
        "magic": MAGIC,
        "version": VERSION,
        "when": now(),
        "next_id": if what.list { next_id } else { 0 },
        "items": if what.list {
            serde_json::to_value(items).unwrap_or(serde_json::Value::Array(Vec::new()))
        } else {
            serde_json::Value::Array(Vec::new())
        },
        "columns": what.list.then(|| columns.cloned()).flatten(),
        "settings": if what.settings {
            serde_json::to_value(settings).unwrap_or(serde_json::Value::Null)
        } else {
            serde_json::Value::Null
        },
        "queues": if what.list { queues } else { &[] as &[Queue] },
        "files": [],
    })
}

/// Take a backup on a worker thread and write it to `dest`, the window watching `job`'s
/// steps (Options → Backup's progress bar). The window's thread hands over what it has
/// (a running download's `live` does not clone, so the list goes as its stored JSON):
/// `snapshot` the whole state as the backup's JSON (its `files` empty), `dests` the
/// unfinished downloads' destinations to gather, and `settings`.
pub fn take_job(snapshot: serde_json::Value, dests: Vec<std::path::PathBuf>, dest: PathBuf) -> Job {
    let job = Job::new();
    let j = job.clone();
    std::thread::spawn(move || {
        j.set(Step::Work {
            done: 0,
            total: 0,
            what: "read",
        });
        let mut backup = match serde_json::from_value::<Backup>(snapshot) {
            Ok(b) => b,
            Err(e) => {
                j.set(Step::Done(Err(e.to_string())));
                return;
            }
        };
        // The unfinished files' parts: read one per step, off the disk.
        let total = dests.len();
        let mut files = Vec::new();
        let mut skipped = Vec::new();
        for (nth, dest) in dests.into_iter().enumerate() {
            j.set(Step::Work {
                done: nth,
                total,
                what: "files",
            });
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
        backup.files = files;
        j.set(Step::Work {
            done: 0,
            total: 1,
            what: "write",
        });
        let text = match serde_json::to_vec_pretty(&backup) {
            Ok(text) => text,
            Err(e) => {
                j.set(Step::Done(Err(e.to_string())));
                return;
            }
        };
        j.set(Step::Work {
            done: 1,
            total: 1,
            what: "write",
        });
        match std::fs::write(&dest, text) {
            Ok(()) => j.set(Step::Done(Ok(if skipped.is_empty() {
                String::new()
            } else {
                format!("Skipped (could not be read):\n{}", skipped.join("\n"))
            }))),
            Err(e) => j.set(Step::Done(Err(e.to_string()))),
        }
    });
    job
}

/// Restore a backup from `path` on a worker thread, the window watching `job`'s steps.
/// `what` says what the user asked back; when it is done the list is reloaded from disk by
/// `reload` (on the window's side, once it sees the job is over).
pub fn restore_job(path: PathBuf, what: What) -> Job {
    let job = Job::new();
    let j = job.clone();
    std::thread::spawn(move || {
        j.set(Step::Work {
            done: 0,
            total: 0,
            what: "read",
        });
        let backup = match Backup::load(&path) {
            Ok(b) => b,
            Err(e) => {
                j.set(Step::Done(Err(e)));
                return;
            }
        };
        let total = backup.files.len();
        j.set(Step::Work {
            done: 0,
            total,
            what: "files",
        });
        match backup.restore(what, |done, _| {
            j.set(Step::Work {
                done,
                total,
                what: "files",
            })
        }) {
            Ok((_, skipped)) => j.set(Step::Done(Ok(if skipped.is_empty() {
                String::new()
            } else {
                skipped.join("\n")
            }))),
            Err(e) => j.set(Step::Done(Err(e))),
        }
    });
    job
}
