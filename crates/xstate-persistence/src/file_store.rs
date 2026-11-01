//! [`FileStore`]: one `<encoded-key>.xsm.json` file per key, atomic writes, advisory locks.
//!
//! Byte-compatible with Python's `FileStore` (`persistence/file_store.py`, ADR-0003): the same
//! file names, the same record layout and the same lock files, so a directory can be shared by
//! both runtimes.
//!
//! ```text
//! {"format":1,"key":...,"machine_version":...,"snapshot":"<json text>","version":n,
//!  "updated_at":f,"deadlines":[{"state_id":..,"entry_seq":..,"due_at_wall":..,"delay_ms":..,
//!  "event_type":..}]}
//! ```
//!
//! Not safe on network shares (SMB / NFS advisory locking is unreliable and rename is not
//! atomic across them). [`StateStore::due_keys`] is unsupported: the timer scanner falls back to
//! loading every record.

use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use xstate_core::snapshot::Deadline;

use crate::memory_store::{
    KeyGuard, KeyLocks, StorePolicy, check_record_fields, deadline_after, deadline_from_value,
};
use crate::store::Result;
use crate::store::{
    ForgetReport, Health, LockGuard, SaveOptions, StateStore, StoreError, StoredSnapshot,
    validate_key, wall_now,
};

/// Record format version written by this library (distinct from the per-key record `version`).
/// A record with a newer format is refused as [`StoreError::Corrupt`], never guessed at.
pub const FORMAT_VERSION: u32 = 1;

const SUFFIX: &str = ".xsm.json";
const LOCK_SUFFIX: &str = ".lock";
const TMP_PREFIX: &str = ".tmp-";
/// Longest encoded stem used verbatim; longer names become `~<sha256 hex>`.
const MAX_STEM: usize = 200;
const HASHED_PREFIX: char = '~';
const HEX_LOWER: &[u8; 16] = b"0123456789abcdef";
const WRITE_RETRIES: u32 = 20;
const WRITE_RETRY_SLEEP: Duration = Duration::from_millis(25);
/// Envelope + deadlines allowance on top of the escaped snapshot when bounding a read.
const RECORD_HEADROOM: usize = 1024 * 1024;
/// How long `save` waits for the key's lock.
const SAVE_LOCK_TIMEOUT: Duration = Duration::from_secs(10);
const LOCK_POLL: Duration = Duration::from_millis(5);
/// Width of the holder-info block written after the locked byte 0 of a lock file.
const HOLDER_INFO_LEN: usize = 63;
const WINDOWS_RESERVED: [&str; 22] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

// ---------------------------------------------------------------------------------------
// Key encoding
// ---------------------------------------------------------------------------------------

fn is_safe(c: char) -> bool {
    matches!(c, 'a'..='z' | '0'..='9' | '-' | '_')
}

/// Filesystem-safe, reversible encoding of `key` (`encode_key` in Python).
///
/// Lower-case letters, digits, `-` and `_` pass through; every other code point (upper case
/// included, so `Order` and `order` never collide on a case-insensitive filesystem) becomes
/// `%xx` per UTF-8 byte with lower-case hex. Windows device names are prefixed with `%5f`.
#[must_use]
pub fn encode_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    let mut buf = [0u8; 4];
    for c in key.chars() {
        if is_safe(c) {
            out.push(c);
        } else {
            for b in c.encode_utf8(&mut buf).bytes() {
                out.push('%');
                out.push(char::from(HEX_LOWER[usize::from(b >> 4)]));
                out.push(char::from(HEX_LOWER[usize::from(b & 0x0f)]));
            }
        }
    }
    if WINDOWS_RESERVED.contains(&out.as_str()) {
        out.insert_str(0, "%5f");
    }
    out
}

/// Inverse of [`encode_key`]; `None` when `name` is not a valid encoding (not one of ours).
#[must_use]
pub fn decode_key(name: &str) -> Option<String> {
    let name = match name.strip_prefix("%5f") {
        Some(rest) if WINDOWS_RESERVED.contains(&rest) => rest,
        _ => name,
    };
    let bytes = name.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The on-disk stem for `key`: [`encode_key`] when it fits in 200 characters, otherwise `~`
/// followed by the SHA-256 hex digest of the key's UTF-8 bytes (`_file_stem` in Python).
#[must_use]
pub fn file_stem(key: &str) -> String {
    let encoded = encode_key(key);
    if encoded.len() <= MAX_STEM {
        return encoded;
    }
    format!("{HASHED_PREFIX}{}", hex::encode(Sha256::digest(key.as_bytes())))
}

fn validate_file_key(key: &str) -> Result<()> {
    validate_key(key)?;
    if key == "." || key == ".." || key.contains('/') || key.contains('\\') {
        return Err(StoreError::InvalidKey(format!(
            "FileStore key {key:?} must not contain path separators or be '.' / '..'"
        )));
    }
    if Path::new(key).is_absolute() {
        return Err(StoreError::InvalidKey(format!("FileStore key {key:?} must not be a path")));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Record encoding (Python `json.dump(rec, separators=(",", ":"))`, ensure_ascii)
// ---------------------------------------------------------------------------------------

/// A compact JSON formatter that escapes everything outside printable ASCII as `\uXXXX`
/// (lower-case hex, surrogate pairs for astral code points), like Python's default
/// `ensure_ascii=True`.
struct AsciiFormatter;

impl serde_json::ser::Formatter for AsciiFormatter {
    fn write_string_fragment<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        fragment: &str,
    ) -> io::Result<()> {
        let bytes = fragment.as_bytes();
        let mut start = 0;
        for (i, c) in fragment.char_indices() {
            if (' '..='~').contains(&c) {
                continue;
            }
            writer.write_all(&bytes[start..i])?;
            for unit in c.encode_utf16(&mut [0u16; 2]) {
                write!(writer, "\\u{unit:04x}")?;
            }
            start = i + c.len_utf8();
        }
        writer.write_all(&bytes[start..])
    }
}

/// The record as written to disk; field order is part of the layout.
#[derive(Serialize)]
struct RecordOut<'a> {
    format: u32,
    key: &'a str,
    machine_version: &'a str,
    snapshot: &'a str,
    version: u64,
    updated_at: f64,
    deadlines: &'a [Deadline],
}

fn render(record: &RecordOut<'_>) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, AsciiFormatter);
    record
        .serialize(&mut ser)
        .map_err(|e| StoreError::Other(format!("cannot encode record: {e}")))?;
    Ok(buf)
}

/// What a record file decodes to.
#[derive(Debug)]
struct Parsed {
    snapshot: String,
    version: u64,
    machine_version: String,
    updated_at: f64,
    deadlines: Vec<Deadline>,
}

fn parse_record(name: &str, bytes: &[u8]) -> Result<Parsed> {
    let corrupt = |why: String| StoreError::Corrupt(format!("FileStore record {name} {why}"));
    let rec: Value =
        serde_json::from_slice(bytes).map_err(|e| corrupt(format!("is not valid JSON: {e}")))?;
    let Some(map) = rec.as_object() else {
        return Err(corrupt("is malformed.".to_owned()));
    };
    if map.get("version").and_then(Value::as_u64).is_none_or(|v| v < 1) {
        return Err(corrupt("is malformed.".to_owned()));
    }
    // 🛡 records before the `format` field are format 1; a newer format is refused.
    let format = match map.get("format") {
        None => 1,
        Some(v) => v
            .as_u64()
            .filter(|f| *f >= 1)
            .ok_or_else(|| corrupt(format!("has an invalid 'format' {v}.")))?,
    };
    if format > u64::from(FORMAT_VERSION) {
        return Err(corrupt(format!(
            "is format {format}, newer than this library supports ({FORMAT_VERSION}). Upgrade xstate-persistence."
        )));
    }
    let empty = Value::String(String::new());
    let fields = check_record_fields(
        &format!("FileStore record {name}"),
        map.get("snapshot").unwrap_or(&empty),
        &map["version"],
        map.get("machine_version").unwrap_or(&empty),
        map.get("updated_at").unwrap_or(&Value::from(0.0)),
    )?;
    let deadlines = match map.get("deadlines") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|d| {
                deadline_from_value(d).map_err(|why| {
                    StoreError::Corrupt(format!(
                        "FileStore record {name}: bad deadline entry: {why}."
                    ))
                })
            })
            .collect::<Result<Vec<_>>>()?,
        Some(_) => {
            return Err(StoreError::Corrupt(format!(
                "FileStore record {name}: 'deadlines' is not a list."
            )));
        }
    };
    Ok(Parsed {
        snapshot: fields.snapshot,
        version: fields.version,
        machine_version: fields.machine_version,
        updated_at: fields.updated_at,
        deadlines,
    })
}

// ---------------------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------------------

/// Tuning for [`FileStore::open_with`].
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct FileStoreOptions {
    /// Age after which an orphaned `.tmp-*` file (left by a killed writer) is swept at open.
    /// Lock files need no reclaiming: the OS releases an advisory lock when its owner dies.
    pub stale_lock_after: Duration,
    /// `fsync` the temp file before the atomic rename so a power loss cannot leave a
    /// zero-length record (default `true`).
    pub fsync: bool,
    /// Snapshot size cap in bytes (default 1 MiB).
    pub max_snapshot_bytes: usize,
}

impl Default for FileStoreOptions {
    fn default() -> Self {
        Self {
            stale_lock_after: Duration::from_secs(60),
            fsync: true,
            max_snapshot_bytes: crate::store::DEFAULT_MAX_SNAPSHOT_BYTES,
        }
    }
}

impl FileStoreOptions {
    /// Set [`stale_lock_after`](Self::stale_lock_after).
    #[must_use]
    pub fn stale_lock_after(mut self, age: Duration) -> Self {
        self.stale_lock_after = age;
        self
    }

    /// Set [`fsync`](Self::fsync).
    #[must_use]
    pub fn fsync(mut self, fsync: bool) -> Self {
        self.fsync = fsync;
        self
    }

    /// Set [`max_snapshot_bytes`](Self::max_snapshot_bytes).
    #[must_use]
    pub fn max_snapshot_bytes(mut self, max: usize) -> Self {
        self.max_snapshot_bytes = max;
        self
    }
}

// ---------------------------------------------------------------------------------------
// OS advisory lock
// ---------------------------------------------------------------------------------------

/// Why an OS-level lock could not be taken.
#[derive(Debug)]
enum OsLockError {
    Timeout { holder: Option<String> },
    Io(io::Error),
}

impl OsLockError {
    fn into_store(self, key: &str, timeout: Duration) -> StoreError {
        match self {
            Self::Timeout { holder } => {
                StoreError::LockTimeout { key: key.to_owned(), timeout, holder }
            }
            Self::Io(e) => StoreError::Io(e),
        }
    }
}

fn holder_info(lock_path: &Path) -> Option<String> {
    let mut file = File::open(lock_path).ok()?;
    file.seek(SeekFrom::Start(1)).ok()?;
    let mut buf = Vec::new();
    file.take(HOLDER_INFO_LEN as u64).read_to_end(&mut buf).ok()?;
    let info = String::from_utf8_lossy(&buf).trim().to_owned();
    (!info.is_empty()).then_some(info)
}

fn open_lock_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

/// Run `f` while holding the exclusive advisory lock on `lock_path` (byte 0, like Python's
/// `flock` / `msvcrt.locking`), polling until `deadline`. The holder's pid and time go after the
/// locked byte for diagnosis by waiters.
fn with_os_lock<T>(
    lock_path: &Path,
    deadline: Instant,
    f: impl FnOnce() -> T,
) -> std::result::Result<T, OsLockError> {
    let file = open_lock_file(lock_path).map_err(OsLockError::Io)?;
    let mut lock = fd_lock::RwLock::new(file);
    loop {
        match lock.try_write() {
            Ok(mut guard) => {
                let info = format!("{} {:.3}\n", std::process::id(), wall_now());
                let _ = guard
                    .seek(SeekFrom::Start(1))
                    .and_then(|_| guard.write_all(format!("{info:<HOLDER_INFO_LEN$}").as_bytes()))
                    .and_then(|()| guard.seek(SeekFrom::Start(0)).map(|_| ()));
                return Ok(f());
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(OsLockError::Timeout { holder: holder_info(lock_path) });
                }
                thread::sleep(LOCK_POLL);
            }
            Err(e) => return Err(OsLockError::Io(e)),
        }
    }
}

/// A held key lock: the in-process gate plus a helper thread that owns the OS lock (the
/// `fd-lock` guard borrows its lock and so cannot be stored directly).
#[derive(Debug)]
struct FileLockGuard {
    key: String,
    release: Option<mpsc::Sender<()>>,
    holder: Option<JoinHandle<()>>,
    _gate: KeyGuard,
}

impl LockGuard for FileLockGuard {
    fn key(&self) -> &str {
        &self.key
    }
}

impl Drop for FileLockGuard {
    fn drop(&mut self) {
        drop(self.release.take()); // closes the channel: the holder thread unlocks and exits
        if let Some(handle) = self.holder.take() {
            let _ = handle.join();
        }
    }
}

// ---------------------------------------------------------------------------------------
// FileStore
// ---------------------------------------------------------------------------------------

/// One `<encoded-key>.xsm.json` file per key under a directory.
///
/// Each write goes to a temp file in the same directory, is flushed (and `fsync`ed unless
/// disabled), then renamed over the target, so readers see the old record or the new one, never a
/// torn one. `save` holds the key's lock (an in-process gate for same-process waiters, then an
/// advisory OS lock for other processes) around read-compare-write, so two writers cannot both
/// pass an `expected_version` check. On Unix the directory is `0700` and files are `0600`.
#[derive(Debug)]
pub struct FileStore {
    dir: PathBuf,
    fsync: bool,
    policy: StorePolicy,
    locks: KeyLocks,
}

impl FileStore {
    /// Open (creating if missing) the store directory with default options.
    ///
    /// # Errors
    /// [`StoreError::Io`] when the directory cannot be created.
    pub fn open(directory: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(directory, FileStoreOptions::default())
    }

    /// Open (creating if missing) the store directory.
    ///
    /// # Errors
    /// [`StoreError::Io`] when the directory cannot be created.
    pub fn open_with(directory: impl AsRef<Path>, options: FileStoreOptions) -> Result<Self> {
        let dir = directory.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
        }
        let store = Self {
            dir,
            fsync: options.fsync,
            policy: StorePolicy::with_max_snapshot_bytes(options.max_snapshot_bytes),
            locks: KeyLocks::default(),
        };
        store.sweep_orphan_temps(options.stale_lock_after);
        Ok(store)
    }

    /// The store directory.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.dir
    }

    /// Remove `.tmp-*` files a killed writer left behind (older than `max_age`, so a live
    /// writer in another process is never raced).
    fn sweep_orphan_temps(&self, max_age: Duration) {
        let Some(cutoff) = SystemTime::now().checked_sub(max_age) else { return };
        let Ok(entries) = fs::read_dir(&self.dir) else { return };
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().starts_with(TMP_PREFIX) {
                continue;
            }
            if entry.metadata().and_then(|m| m.modified()).is_ok_and(|m| m < cutoff) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    fn path(&self, key: &str) -> Result<PathBuf> {
        validate_file_key(key)?;
        Ok(self.dir.join(format!("{}{SUFFIX}", file_stem(key))))
    }

    fn lock_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{}{LOCK_SUFFIX}", file_stem(key)))
    }

    /// Largest legitimate record file: the snapshot JSON-escaped (`\uXXXX` is at most 6 bytes
    /// per input byte) plus headroom for the envelope and deadlines.
    fn record_limit(&self) -> usize {
        self.policy.max_snapshot_bytes().saturating_mul(6).saturating_add(RECORD_HEADROOM)
    }

    /// Read a record file, bounded by the record limit; `None` when it does not exist.
    fn read_bytes(&self, path: &Path) -> Result<Option<Vec<u8>>> {
        let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let limit = self.record_limit();
        for attempt in 0..WRITE_RETRIES {
            match File::open(path) {
                Ok(file) => {
                    let mut buf = Vec::new();
                    file.take(limit as u64 + 1).read_to_end(&mut buf).map_err(|e| {
                        StoreError::Corrupt(format!("FileStore record {name} cannot be read: {e}"))
                    })?;
                    if buf.len() > limit {
                        return Err(StoreError::SnapshotTooLarge {
                            key: name,
                            size: buf.len(),
                            limit,
                        });
                    }
                    return Ok(Some(buf));
                }
                Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
                // 🪟 a rename in flight (or a directory where the record should be) reads as
                // PermissionDenied on Windows: retry briefly, as the writer does.
                Err(e) if e.kind() == ErrorKind::PermissionDenied => {
                    if path.is_dir() {
                        return Err(StoreError::Corrupt(format!(
                            "FileStore record {name} is a directory."
                        )));
                    }
                    if attempt + 1 == WRITE_RETRIES {
                        return Err(StoreError::Io(e));
                    }
                    thread::sleep(WRITE_RETRY_SLEEP);
                }
                Err(e) => {
                    return Err(StoreError::Corrupt(format!(
                        "FileStore record {name} cannot be read: {e}"
                    )));
                }
            }
        }
        Err(StoreError::Other(format!("FileStore record {name} stayed unreadable")))
    }

    fn read_record(&self, path: &Path) -> Result<Option<Parsed>> {
        let Some(bytes) = self.read_bytes(path)? else { return Ok(None) };
        let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        parse_record(&name, &bytes).map(Some)
    }

    /// Write `bytes` to a temp file next to `path`, then rename it over `path`.
    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let tmp = self.dir.join(format!("{TMP_PREFIX}{:016x}{SUFFIX}", rand::random::<u64>()));
        let result = self.write_then_rename(&tmp, path, bytes);
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    fn write_then_rename(&self, tmp: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
        {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            let mut file = options.open(tmp)?;
            file.write_all(bytes)?;
            file.flush()?;
            if self.fsync {
                file.sync_all()?;
            }
        }
        // 🪟 a reader mid-open can make the rename fail transiently on Windows.
        for attempt in 0..WRITE_RETRIES {
            match fs::rename(tmp, path) {
                Err(e)
                    if e.kind() == ErrorKind::PermissionDenied && attempt + 1 < WRITE_RETRIES =>
                {
                    thread::sleep(WRITE_RETRY_SLEEP);
                }
                other => return other,
            }
        }
        Ok(())
    }

    /// Run `f` under the key's write lock unless this thread already holds it via
    /// [`StateStore::lock`] (the OS lock is not reentrant).
    fn with_write_lock<T>(&self, key: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
        if self.locks.held_by_current(key) {
            return f();
        }
        let deadline = deadline_after(SAVE_LOCK_TIMEOUT);
        let lock_path = self.lock_path(key);
        let _gate =
            self.locks.acquire(key, SAVE_LOCK_TIMEOUT).ok_or_else(|| StoreError::LockTimeout {
                key: key.to_owned(),
                timeout: SAVE_LOCK_TIMEOUT,
                holder: holder_info(&lock_path),
            })?;
        with_os_lock(&lock_path, deadline, f).map_err(|e| e.into_store(key, SAVE_LOCK_TIMEOUT))?
    }

    fn key_from_name(&self, path: &Path, name: &str) -> Option<String> {
        let stem = name.strip_suffix(SUFFIX)?;
        if name.starts_with(TMP_PREFIX) {
            return None;
        }
        let key = if stem.starts_with(HASHED_PREFIX) {
            // The key lives in the record body; accept it only if it really hashes to this name.
            let bytes = self.read_bytes(path).ok().flatten()?;
            let parsed = serde_json::from_slice::<Value>(&bytes).ok()?;
            parsed.get("key")?.as_str()?.to_owned()
        } else {
            decode_key(stem)?
        };
        (file_stem(&key) == stem).then_some(key)
    }
}

impl StateStore for FileStore {
    fn load(&self, key: &str) -> Result<Option<StoredSnapshot>> {
        let path = self.path(key)?;
        let Some(rec) = self.read_record(&path)? else { return Ok(None) };
        self.policy.check_loaded(key, &rec.snapshot)?;
        Ok(Some(StoredSnapshot {
            key: key.to_owned(),
            snapshot: rec.snapshot,
            version: rec.version,
            machine_version: rec.machine_version,
            updated_at: rec.updated_at,
            deadlines: rec.deadlines,
        }))
    }

    fn save(&self, key: &str, snapshot_json: &str, options: &SaveOptions<'_>) -> Result<u64> {
        let deadlines = self.policy.prepare_save(key, snapshot_json, options)?;
        let path = self.path(key)?;
        self.with_write_lock(key, || {
            let existing = self.read_record(&path)?.map(|r| r.version);
            let current = existing.unwrap_or(0);
            if let Some(expected) = options.expected_version.filter(|e| *e != current) {
                return Err(StoreError::Conflict {
                    key: key.to_owned(),
                    expected,
                    actual: existing,
                });
            }
            let version = current + 1;
            let bytes = render(&RecordOut {
                format: FORMAT_VERSION,
                key,
                machine_version: options.machine_version,
                snapshot: snapshot_json,
                version,
                updated_at: wall_now(),
                deadlines: &deadlines,
            })?;
            self.write_atomic(&path, &bytes).map_err(|e| {
                let name =
                    path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                StoreError::Io(io::Error::new(
                    e.kind(),
                    format!("FileStore could not write {name}: {e}"),
                ))
            })?;
            Ok(version)
        })
    }

    fn delete(&self, key: &str) -> Result<bool> {
        match fs::remove_file(self.path(key)?) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    fn forget(&self, key: &str) -> Result<ForgetReport> {
        let existed = self.delete(key)?;
        let locks = u64::from(fs::remove_file(self.lock_path(key)).is_ok());
        Ok(ForgetReport {
            snapshots: u64::from(existed),
            deadlines: 0,
            other: vec![("locks".to_owned(), locks)],
        })
    }

    fn list_keys(&self, prefix: &str, limit: usize) -> Result<Vec<String>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut keys = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else { continue };
            if let Some(key) =
                self.key_from_name(&entry.path(), name).filter(|k| k.starts_with(prefix))
            {
                keys.push(key);
            }
        }
        keys.sort_unstable();
        keys.truncate(limit);
        keys.retain(|k| validate_file_key(k).is_ok());
        Ok(keys)
    }

    fn lock(&self, key: &str, timeout: Duration) -> Result<Box<dyn LockGuard>> {
        validate_file_key(key)?;
        let deadline = deadline_after(timeout);
        let lock_path = self.lock_path(key);
        let timed_out = || StoreError::LockTimeout {
            key: key.to_owned(),
            timeout,
            holder: holder_info(&lock_path),
        };
        let gate = self.locks.acquire(key, timeout).ok_or_else(timed_out)?;

        let (ready_tx, ready_rx) = mpsc::channel::<std::result::Result<(), OsLockError>>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let path = lock_path.clone();
        let holder = thread::Builder::new().name("xsm-file-lock".to_owned()).spawn(move || {
            let held = with_os_lock(&path, deadline, || {
                let _ = ready_tx.send(Ok(()));
                let _ = release_rx.recv(); // returns when the guard drops its sender
            });
            if let Err(e) = held {
                let _ = ready_tx.send(Err(e));
            }
        })?;
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = holder.join();
                return Err(e.into_store(key, timeout));
            }
            Err(_) => {
                let _ = holder.join();
                return Err(StoreError::Other("file lock helper thread failed".to_owned()));
            }
        }
        Ok(Box::new(FileLockGuard {
            key: key.to_owned(),
            release: Some(release_tx),
            holder: Some(holder),
            _gate: gate,
        }))
    }

    fn health(&self) -> Result<Health> {
        let ok = fs::metadata(&self.dir).is_ok_and(|m| m.is_dir() && !m.permissions().readonly());
        Ok(Health {
            ok,
            backend: "file",
            details: vec![("directory".to_owned(), self.dir.display().to_string())],
        })
    }

    fn max_snapshot_bytes(&self) -> usize {
        self.policy.max_snapshot_bytes()
    }
}
