//! `xsm snapshots`: list what a store holds and find instances a deploy would strand.
//!
//! The context of a snapshot is user data and never leaves the store through this command: only
//! the key, record version, machine labels, status, active states and age are read or printed.

use std::path::PathBuf;

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use xstate_persistence::store::wall_now;
use xstate_persistence::{FileStore, SqliteStore, StateStore};

use super::analysis::analyse;
use crate::output::{self, EXIT_FINDINGS, EXIT_OK, print_json};
use crate::ui::{Cell, Role, Table, Ui, print_lines};

/// Arguments of `snapshots`.
#[derive(Debug, clap::Args)]
pub(crate) struct SnapshotsArgs {
    /// Store to inspect: `sqlite:///path/to.db` or `file:///path/to/dir`.
    #[arg(long, value_name = "URL")]
    store: String,
    /// Machine JSON whose structure hash is the reference for `--stale`.
    #[arg(value_name = "MACHINE")]
    file: Option<PathBuf>,
    /// Only list records written by a different version of the machine.
    #[arg(long)]
    stale: bool,
    /// Exit 1 when any stale record exists (implies `--stale`).
    #[arg(long)]
    fail_if_stale: bool,
    /// Only keys starting with this prefix.
    #[arg(long, default_value = "", value_name = "PREFIX")]
    prefix: String,
    /// Maximum rows to show (`--stale` still scans every key).
    #[arg(long, default_value_t = 1000, value_name = "N")]
    limit: usize,
    /// Emit JSON instead of a table.
    #[arg(long)]
    json: bool,
}

/// A store location parsed from a URL.
#[derive(Debug, PartialEq, Eq)]
enum StoreSpec {
    Sqlite(PathBuf),
    File(PathBuf),
}

/// Decode `%XX` escapes (invalid escapes are kept as written).
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse `sqlite:///rel.db`, `sqlite:////abs/x.db`, `sqlite:///C:/x.db` and `file:///dir`.
///
/// Three slashes after `sqlite:` mean a relative path and four an absolute one (the `SQLAlchemy`
/// rule, shared with the Python CLI); `file:` paths stay absolute. A Windows drive letter
/// (`/C:/x`) loses its leading slash.
fn parse_store_url(url: &str) -> Result<StoreSpec, String> {
    let Some((scheme, rest)) = url.split_once("://") else {
        return Err(format!("unsupported store URL {url:?}: use sqlite:///path.db or file:///dir"));
    };
    let scheme = scheme.to_ascii_lowercase();
    if scheme == "memory" {
        return Err("memory:// is a new empty store in this process".into());
    }
    if scheme != "sqlite" && scheme != "file" {
        return Err(format!("unsupported store URL {url:?}: use sqlite:///path.db or file:///dir"));
    }
    let has_netloc = !rest.is_empty() && !rest.starts_with('/');
    let mut path = percent_decode(rest);
    if has_netloc {
        path = format!("//{path}");
    }
    let bytes = path.as_bytes();
    let drive_letter = bytes.len() >= 3 && bytes[0] == b'/' && bytes[2] == b':';
    let sqlite_relative = scheme == "sqlite" && path.starts_with('/') && !has_netloc;
    if drive_letter || sqlite_relative {
        path.remove(0);
    }
    if path.is_empty() {
        return Err(format!("store URL {url:?} has no path"));
    }
    let path = PathBuf::from(path);
    Ok(if scheme == "sqlite" { StoreSpec::Sqlite(path) } else { StoreSpec::File(path) })
}

/// Open an existing store; a typo must never silently create an empty one.
fn open_store(spec: &StoreSpec) -> Result<Box<dyn StateStore>> {
    match spec {
        StoreSpec::Sqlite(path) => {
            if !path.is_file() {
                return Err(output::usage(format!("no such database file: {}", path.display())));
            }
            let store = SqliteStore::open(path)
                .map_err(|e| output::usage(format!("cannot open {}: {e}", path.display())))?;
            Ok(Box::new(store))
        }
        StoreSpec::File(path) => {
            if !path.is_dir() {
                return Err(output::usage(format!("no such directory: {}", path.display())));
            }
            let store = FileStore::open(path)
                .map_err(|e| output::usage(format!("cannot open {}: {e}", path.display())))?;
            Ok(Box::new(store))
        }
    }
}

/// How a record relates to the reference machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Freshness {
    /// Same machine, same structure hash.
    Current,
    /// Same machine id, different structure hash: restoring would be refused.
    Stale,
    /// Written by a machine with another id; not this machine's concern.
    Other,
    /// No reference machine, or the record could not be read.
    Unknown,
}

/// The machine a record is compared against.
struct Reference {
    id: String,
    hash: String,
}

/// One listed record. Never carries the context.
#[derive(Debug, Serialize)]
struct Row {
    key: String,
    version: u64,
    machine_version: Option<String>,
    status: Option<String>,
    state_ids: Vec<String>,
    age_s: f64,
    deadlines: usize,
    machine_id: Option<String>,
    machine_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stale: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip)]
    freshness: Option<Freshness>,
}

fn string_field(doc: &Value, name: &str) -> Option<String> {
    doc.get(name).and_then(Value::as_str).map(str::to_owned)
}

fn classify(row: &Row, reference: Option<&Reference>) -> Freshness {
    let Some(reference) = reference else { return Freshness::Unknown };
    match (&row.machine_id, &row.machine_hash) {
        (Some(id), _) if *id != reference.id => Freshness::Other,
        (Some(_), hash) if hash.as_deref() == Some(reference.hash.as_str()) => Freshness::Current,
        (Some(_), _) => Freshness::Stale,
        _ => Freshness::Unknown,
    }
}

fn load_row(store: &dyn StateStore, key: &str, now: f64, reference: Option<&Reference>) -> Row {
    let mut row = Row {
        key: key.to_owned(),
        version: 0,
        machine_version: None,
        status: None,
        state_ids: Vec::new(),
        age_s: 0.0,
        deadlines: 0,
        machine_id: None,
        machine_hash: None,
        stale: None,
        error: None,
        freshness: None,
    };
    match store.load(key) {
        Ok(Some(record)) => {
            row.version = record.version;
            row.machine_version = Some(record.machine_version).filter(|v| !v.is_empty());
            row.age_s = (now - record.updated_at).max(0.0);
            row.deadlines = record.deadlines.len();
            // Only these fields are read: the context is user data and stays in the store.
            match serde_json::from_str::<Value>(&record.snapshot) {
                Ok(doc) => {
                    row.status = string_field(&doc, "status");
                    row.machine_id = string_field(&doc, "machine_id");
                    row.machine_hash = string_field(&doc, "machine_hash");
                    row.state_ids = doc
                        .get("state_ids")
                        .and_then(Value::as_array)
                        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                        .unwrap_or_default();
                }
                Err(e) => row.error = Some(format!("unreadable snapshot: {e}")),
            }
        }
        Ok(None) => row.error = Some("record vanished while listing".to_owned()),
        Err(e) => row.error = Some(e.to_string()),
    }
    let freshness =
        if row.error.is_some() { Freshness::Unknown } else { classify(&row, reference) };
    if reference.is_some() && freshness != Freshness::Unknown {
        row.stale = Some(freshness == Freshness::Stale);
    }
    row.freshness = Some(freshness);
    row
}

/// Compact age: `12s`, `5m`, `3.0h`, `2.0d`.
fn age_text(seconds: f64) -> String {
    if seconds < 90.0 {
        format!("{seconds:.0}s")
    } else if seconds < 5400.0 {
        format!("{:.0}m", seconds / 60.0)
    } else if seconds < 172_800.0 {
        format!("{:.1}h", seconds / 3600.0)
    } else {
        format!("{:.1}d", seconds / 86_400.0)
    }
}

/// Run `snapshots`.
pub(crate) fn run(ui: Ui, args: &SnapshotsArgs) -> Result<u8> {
    let stale_only = args.stale || args.fail_if_stale;
    if stale_only && args.file.is_none() {
        return Err(output::usage("--stale needs the machine JSON to compare against"));
    }
    let reference = match &args.file {
        Some(file) => {
            let analysis = analyse(ui, file, true, false);
            let Some(machine) = analysis.machine else {
                return Err(output::usage(format!(
                    "cannot read machine JSON: {}",
                    analysis.first_error()
                )));
            };
            Some(Reference {
                id: machine.id().to_owned(),
                hash: machine.structure_hash().to_owned(),
            })
        }
        None => None,
    };
    let spec = parse_store_url(&args.store).map_err(output::usage)?;
    let store = open_store(&spec)?;
    let keys = store
        .list_keys(&args.prefix, usize::MAX)
        .map_err(|e| output::usage(format!("cannot list {}: {e}", args.store)))?;
    ui.verbose(&format!("{} key(s) match prefix {:?}", keys.len(), args.prefix));
    let now = wall_now();
    let (rows, total) =
        collect_rows(store.as_ref(), &keys, args.limit, stale_only, now, reference.as_ref());
    if args.json {
        print_json(&json_report(args, reference.as_ref(), stale_only, &rows, total))?;
    } else {
        print_table(ui, args, reference.as_ref(), stale_only, &rows, total);
    }
    Ok(if args.fail_if_stale && total > 0 { EXIT_FINDINGS } else { EXIT_OK })
}

/// Rows to show and how many records matched in total.
fn collect_rows(
    store: &dyn StateStore,
    keys: &[String],
    limit: usize,
    stale_only: bool,
    now: f64,
    reference: Option<&Reference>,
) -> (Vec<Row>, usize) {
    if !stale_only {
        let rows = keys.iter().take(limit).map(|k| load_row(store, k, now, reference)).collect();
        return (rows, keys.len());
    }
    // Staleness is only known after loading, so every key is scanned; `limit` caps the output.
    let stale: Vec<Row> = keys
        .iter()
        .map(|k| load_row(store, k, now, reference))
        .filter(|r| r.freshness == Some(Freshness::Stale))
        .collect();
    let total = stale.len();
    (stale.into_iter().take(limit).collect(), total)
}

fn json_report(
    args: &SnapshotsArgs,
    reference: Option<&Reference>,
    stale_only: bool,
    rows: &[Row],
    total: usize,
) -> Value {
    serde_json::json!({
        "store": args.store,
        "machine": reference.map(|r| r.id.as_str()),
        "machine_hash": reference.map(|r| r.hash.as_str()),
        "stale_only": stale_only,
        "count": rows.len(),
        "total": total,
        "truncated": total > rows.len(),
        "snapshots": rows,
    })
}

fn hash_cell(row: &Row) -> Cell {
    match row.freshness {
        Some(Freshness::Current) => Cell::styled(Role::Ok, "ok"),
        Some(Freshness::Stale) => Cell::styled(Role::Err, "STALE"),
        Some(Freshness::Other) => Cell::styled(Role::Muted, "other machine"),
        _ => Cell::styled(Role::Muted, "-"),
    }
}

fn print_table(
    ui: Ui,
    args: &SnapshotsArgs,
    reference: Option<&Reference>,
    stale_only: bool,
    rows: &[Row],
    total: usize,
) {
    println!();
    if rows.is_empty() {
        let message = if stale_only { "no stale snapshots" } else { "store is empty" };
        println!("{}", ui.paint(Role::Muted, message));
        println!();
        return;
    }
    let mut headers = vec!["Key", "Ver", "Machine", "Status", "State", "Age"];
    if reference.is_some() {
        headers.push("Hash");
    }
    let mut table = Table::new(&headers).right(1);
    for row in rows {
        let states: Vec<&str> =
            row.state_ids.iter().map(|s| s.rsplit('.').next().unwrap_or(s)).collect();
        let status_text = row.error.as_deref().map_or_else(
            || row.status.clone().unwrap_or_else(|| "?".to_owned()),
            |e| format!("error: {e}"),
        );
        let mut cells = vec![
            Cell::text(row.key.clone()),
            Cell::text(row.version.to_string()),
            Cell::text(row.machine_version.clone().unwrap_or_else(|| "-".to_owned())),
            Cell::text(status_text),
            Cell::text(states.join(", ")),
            Cell::text(age_text(row.age_s)),
        ];
        if reference.is_some() {
            cells.push(hash_cell(row));
        }
        table.add(cells);
    }
    let title = if stale_only { "stale snapshots" } else { "snapshots" };
    let shown =
        if total == rows.len() { total.to_string() } else { format!("{} of {total}", rows.len()) };
    println!("{}  {}", ui.rule(&format!("{title} ({shown})")), ui.paint(Role::Muted, &args.store));
    print_lines(&table.render(ui));
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_urls_follow_the_sqlalchemy_slash_rule() {
        assert_eq!(parse_store_url("sqlite:///app.db"), Ok(StoreSpec::Sqlite("app.db".into())));
        assert_eq!(
            parse_store_url("sqlite:////var/app.db"),
            Ok(StoreSpec::Sqlite("/var/app.db".into()))
        );
        assert_eq!(
            parse_store_url("sqlite:///C:/data/app.db"),
            Ok(StoreSpec::Sqlite("C:/data/app.db".into()))
        );
    }

    #[test]
    fn file_urls_stay_absolute_and_decode_escapes() {
        assert_eq!(parse_store_url("file:///var/snaps"), Ok(StoreSpec::File("/var/snaps".into())));
        assert_eq!(parse_store_url("file:///C:/my%20dir"), Ok(StoreSpec::File("C:/my dir".into())));
    }

    #[test]
    fn unsupported_urls_are_refused() {
        assert!(parse_store_url("memory://").is_err());
        assert!(parse_store_url("postgres://x/y").is_err());
        assert!(parse_store_url("app.db").is_err());
        assert!(parse_store_url("sqlite://").is_err());
    }

    #[test]
    fn ages_are_compact() {
        assert_eq!(age_text(12.0), "12s");
        assert_eq!(age_text(600.0), "10m");
        assert_eq!(age_text(7200.0), "2.0h");
        assert_eq!(age_text(259_200.0), "3.0d");
    }
}
