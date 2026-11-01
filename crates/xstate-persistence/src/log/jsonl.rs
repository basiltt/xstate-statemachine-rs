//! Append-only JSON Lines transition log.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use super::record::{TransitionLogStore, TransitionRecord, check_append, check_cutoff};
use crate::store::{Result, StoreError};

/// `(size, mtime)` of the log file; `(0, None)` when absent.
type Signature = (u64, Option<SystemTime>);

#[derive(Debug, Default)]
struct Cache {
    /// Last seq per machine id, valid while the file's signature equals `signature`.
    last_seq: HashMap<String, u64>,
    signature: Option<Signature>,
}

/// A single `.jsonl` file: one sorted-key JSON object per line, appended with one `write` under
/// a process lock. Reads scan the file, which is fine for audit trails, not for millions of
/// rows.
///
/// The last `seq` per key is cached while the file is exactly as this process left it (size
/// and mtime), so `next_seq` is O(1) in the steady state; any outside change forces a rescan.
/// Appends are atomic within one process only: two processes appending the same key to one file
/// can mint the same `seq`. Use [`SqliteLog`](super::SqliteLog) for multi-process writers.
///
/// A torn or corrupt line is a [`StoreError::Corrupt`] naming `path:line`, never skipped.
#[derive(Debug)]
pub struct JsonLinesLog {
    path: PathBuf,
    cache: Mutex<Cache>,
}

impl JsonLinesLog {
    /// A log in `path` (the parent directory is created; the file on first append).
    ///
    /// # Errors
    /// I/O failure creating the directory.
    pub fn new(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Self { path, cache: Mutex::new(Cache::default()) })
    }

    /// The file this log writes to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn cache(&self) -> MutexGuard<'_, Cache> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn signature(&self) -> Result<Signature> {
        match std::fs::metadata(&self.path) {
            Ok(meta) => Ok((meta.len(), meta.modified().ok())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((0, None)),
            Err(e) => Err(e.into()),
        }
    }

    /// Every record in the file, in file order. A BOM and CRLF line ends are tolerated.
    fn scan(&self) -> Result<Vec<TransitionRecord>> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let text = String::from_utf8(bytes).map_err(|e| {
            StoreError::Corrupt(format!("{}: not UTF-8 ({e})", self.path.display()))
        })?;
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        let mut records = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let at = || format!("{}:{}", self.path.display(), index + 1);
            let value: serde_json::Value = serde_json::from_str(line).map_err(|e| {
                StoreError::Corrupt(format!(
                    "{}: not JSON ({e}); a writer killed mid-append leaves a torn last line -- \
                     truncate it to repair",
                    at()
                ))
            })?;
            records.push(
                TransitionRecord::from_value(&value)
                    .map_err(|e| StoreError::Corrupt(format!("{}: {e}", at())))?,
            );
        }
        Ok(records)
    }

    /// Last seq for `machine_id`, rescanning when the file changed under us.
    fn last(&self, cache: &mut Cache, machine_id: &str) -> Result<u64> {
        let signature = self.signature()?;
        if cache.signature != Some(signature) {
            cache.last_seq.clear();
            for record in self.scan()? {
                let slot = cache.last_seq.entry(record.machine_id).or_insert(0);
                *slot = (*slot).max(record.seq);
            }
            cache.signature = Some(signature);
        }
        Ok(cache.last_seq.get(machine_id).copied().unwrap_or(0))
    }

    fn write_line(&self, cache: &mut Cache, record: &TransitionRecord) -> Result<()> {
        let mut line = record.to_json();
        line.push('\n');
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?;
        file.write_all(line.as_bytes())?;
        file.flush()?;
        drop(file);
        let slot = cache.last_seq.entry(record.machine_id.clone()).or_insert(0);
        *slot = (*slot).max(record.seq);
        cache.signature = Some(self.signature()?);
        Ok(())
    }

    /// Rewrite the file keeping the records `keep` accepts; returns how many were dropped. The
    /// old file stays intact until the atomic rename.
    fn rewrite(&self, keep: impl Fn(&TransitionRecord) -> bool) -> Result<u64> {
        let mut cache = self.cache();
        let records = self.scan()?;
        let kept: Vec<&TransitionRecord> = records.iter().filter(|r| keep(r)).collect();
        let tmp = self.path.with_extension("jsonl.tmp");
        let written = (|| -> std::io::Result<()> {
            let mut file = std::fs::File::create(&tmp)?;
            for record in &kept {
                file.write_all(record.to_json().as_bytes())?;
                file.write_all(b"\n")?;
            }
            file.sync_all()
        })();
        if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, &self.path)) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        cache.signature = None;
        Ok(u64::try_from(records.len() - kept.len()).unwrap_or(u64::MAX))
    }
}

impl TransitionLogStore for JsonLinesLog {
    fn append(&self, record: &TransitionRecord) -> Result<()> {
        check_append(record)?;
        let mut cache = self.cache();
        self.write_line(&mut cache, record)
    }

    fn append_next(&self, record: TransitionRecord) -> Result<TransitionRecord> {
        let mut cache = self.cache();
        let seq = self.last(&mut cache, &record.machine_id)? + 1;
        let stored = TransitionRecord { seq, ..record };
        check_append(&stored)?;
        self.write_line(&mut cache, &stored)?;
        Ok(stored)
    }

    fn next_seq(&self, machine_id: &str) -> Result<u64> {
        let mut cache = self.cache();
        Ok(self.last(&mut cache, machine_id)? + 1)
    }

    fn read(
        &self,
        machine_id: &str,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<TransitionRecord>> {
        let _cache = self.cache();
        let mut rows: Vec<TransitionRecord> = self
            .scan()?
            .into_iter()
            .filter(|r| r.machine_id == machine_id && r.seq > after_seq)
            .collect();
        rows.sort_by_key(|r| r.seq);
        rows.truncate(limit);
        Ok(rows)
    }

    fn purge_older_than(&self, cutoff_ts: f64) -> Result<u64> {
        let cutoff = check_cutoff(cutoff_ts)?;
        self.rewrite(|r| r.ts >= cutoff)
    }

    fn forget(&self, machine_id: &str) -> Result<u64> {
        self.rewrite(|r| r.machine_id != machine_id)
    }
}
