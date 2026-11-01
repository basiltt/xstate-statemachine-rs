//! Snapshot migration between chart versions: ship v2 of a machine while instances are mid-flight
//! on v1.
//!
//! No automatic migration is promised, only the two honest things: every snapshot records the
//! chart `machine_version` that wrote it, and a mismatch fails loudly unless an upcaster was
//! registered for exactly that hop. [`SnapshotMigrator`] is a graph of `(from, to)` steps over
//! snapshot JSON blobs; [`migrate`](SnapshotMigrator::migrate) walks the shortest route,
//! rewrites `machine_version` and drops `machine_hash` (the structure changed by definition, and
//! the restore re-derives it against the new machine). Ports Python `persistence/migration.py`.
//!
//! # Not ported
//!
//! * The version *policy* (`on_version_mismatch` = `error` / `warn` / `migrate`) and
//!   `MachineVersionMismatchError`: they belong to the restore path, which the interpreter
//!   owns. Call [`SnapshotMigrator::migrate`] on the blob before restoring it.
//! * Child-actor blobs under `actors` are left as they are, exactly like Python: the restore
//!   re-runs the migration per child against the child's own machine.
//! * A step that rewrites `state_ids` without `configuration` gets `configuration: null`
//!   (Python re-derives it from `state_ids` on restore). [`xstate_core::Snapshot`] cannot
//!   decode a `null` configuration yet, so a Rust restore needs the caller to rebuild it.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::Arc;

use serde_json::Value;

use crate::store::{Result, StoreError};

/// One upcast step: takes the decoded snapshot blob, returns the migrated blob.
pub type MigrationStep = Arc<dyn Fn(Value) -> Result<Value> + Send + Sync>;

fn show(version: Option<&String>) -> String {
    version.map_or_else(|| "None".to_owned(), |v| format!("{v:?}"))
}

/// Migration failures.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MigrationError {
    /// No chain of registered steps leads from `found` to `target`.
    #[error(
        "no migration path for machine '{machine_id}' from version {} to {}",
        show(.found.as_ref()),
        show(.target.as_ref())
    )]
    NoPath {
        /// The machine.
        machine_id: String,
        /// The blob's `machine_version` (`None` = unlabelled).
        found: Option<String>,
        /// The requested version.
        target: Option<String>,
    },
    /// A step raised, returned something that is not a snapshot object, or left `state_ids` and
    /// `configuration` disagreeing.
    #[error("migration step {from:?}->{to:?} for '{machine_id}' failed: {message}")]
    StepFailed {
        /// The machine.
        machine_id: String,
        /// The hop's source version.
        from: String,
        /// The hop's target version.
        to: String,
        /// What went wrong.
        message: String,
    },
    /// A step must change the version.
    #[error("a migration step must change the version (got {version:?} -> {version:?})")]
    SameVersion {
        /// The offending label.
        version: String,
    },
}

impl From<MigrationError> for StoreError {
    fn from(error: MigrationError) -> Self {
        match error {
            MigrationError::StepFailed { .. } => Self::Corrupt(error.to_string()),
            MigrationError::NoPath { .. } | MigrationError::SameVersion { .. } => {
                Self::Other(error.to_string())
            }
        }
    }
}

#[derive(Clone)]
struct Entry {
    machine_id: Option<String>,
    from: String,
    to: String,
    step: MigrationStep,
}

/// A registry of version-to-version upcast steps for snapshot blobs.
///
/// Steps may be scoped to a machine id when one migrator serves several charts; unscoped steps
/// apply to any machine, and a scoped step wins over an unscoped one for the same hop. Labels
/// are plain strings. Registering a hop twice replaces the earlier step (last wins).
#[derive(Clone, Default)]
pub struct SnapshotMigrator {
    entries: Vec<Entry>,
}

impl fmt::Debug for SnapshotMigrator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hops: Vec<String> = self
            .entries
            .iter()
            .map(|e| {
                format!(
                    "{}{}->{}",
                    e.machine_id.as_deref().map_or(String::new(), |m| format!("{m}:")),
                    e.from,
                    e.to
                )
            })
            .collect();
        f.debug_struct("SnapshotMigrator").field("steps", &hops).finish()
    }
}

impl SnapshotMigrator {
    /// An empty migrator.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `step` as the `from -> to` upcaster, optionally scoped to `machine_id`.
    ///
    /// # Errors
    /// [`MigrationError::SameVersion`] when `from == to`.
    pub fn register(
        &mut self,
        from: &str,
        to: &str,
        step: MigrationStep,
        machine_id: Option<&str>,
    ) -> std::result::Result<&mut Self, MigrationError> {
        if from == to {
            return Err(MigrationError::SameVersion { version: from.to_owned() });
        }
        let entry = Entry {
            machine_id: machine_id.map(str::to_owned),
            from: from.to_owned(),
            to: to.to_owned(),
            step,
        };
        match self
            .entries
            .iter_mut()
            .find(|e| e.machine_id == entry.machine_id && e.from == entry.from && e.to == entry.to)
        {
            Some(existing) => *existing = entry,
            None => self.entries.push(entry),
        }
        Ok(self)
    }

    /// The steps applicable to `machine_id` as `(from, to)` hops: unscoped steps in
    /// registration order, with scoped steps replacing same-hop ones and appended otherwise.
    #[must_use]
    pub fn steps_for(&self, machine_id: Option<&str>) -> Vec<(String, String)> {
        self.applicable(machine_id).into_iter().map(|e| (e.from.clone(), e.to.clone())).collect()
    }

    fn applicable(&self, machine_id: Option<&str>) -> Vec<&Entry> {
        let mut out: Vec<&Entry> = self.entries.iter().filter(|e| e.machine_id.is_none()).collect();
        for entry in self
            .entries
            .iter()
            .filter(|e| e.machine_id.is_some() && e.machine_id.as_deref() == machine_id)
        {
            match out.iter_mut().find(|o| o.from == entry.from && o.to == entry.to) {
                Some(slot) => *slot = entry,
                None => out.push(entry),
            }
        }
        out
    }

    /// The shortest chain of `(from, to)` hops over unscoped steps (`Some(vec![])` when
    /// `from == to`), or `None` when there is no route.
    #[must_use]
    pub fn path(&self, from: &str, to: &str) -> Option<Vec<(String, String)>> {
        self.path_for(None, Some(from), Some(to))
    }

    /// [`path`](Self::path) for one machine: steps scoped to `machine_id` count too. An
    /// unlabelled snapshot (`None`) has a route only to another `None`. Among equally short
    /// routes the one through the earliest-registered step wins.
    #[must_use]
    pub fn path_for(
        &self,
        machine_id: Option<&str>,
        from: Option<&str>,
        to: Option<&str>,
    ) -> Option<Vec<(String, String)>> {
        if from == to {
            return Some(Vec::new());
        }
        let (from, to) = (from?, to?);
        let steps = self.applicable(machine_id);
        let mut graph: HashMap<&str, Vec<&str>> = HashMap::new();
        for e in &steps {
            graph.entry(e.from.as_str()).or_default().push(e.to.as_str());
        }
        let mut prev: HashMap<&str, Option<&str>> = HashMap::from([(from, None)]);
        let mut queue = VecDeque::from([from]);
        while let Some(current) = queue.pop_front() {
            if current == to {
                break;
            }
            for &next in graph.get(current).map_or(&[][..], Vec::as_slice) {
                if !prev.contains_key(next) {
                    prev.insert(next, Some(current));
                    queue.push_back(next);
                }
            }
        }
        prev.get(to)?;
        let mut hops = Vec::new();
        let mut current = to;
        while let Some(Some(before)) = prev.get(current) {
            hops.push(((*before).to_owned(), current.to_owned()));
            current = before;
        }
        hops.reverse();
        Some(hops)
    }

    /// `true` when [`path_for`](Self::path_for) finds a route.
    #[must_use]
    pub fn can_migrate(
        &self,
        machine_id: Option<&str>,
        from: Option<&str>,
        to: Option<&str>,
    ) -> bool {
        self.path_for(machine_id, from, to).is_some()
    }

    /// Migrate `blob` (a decoded snapshot) to version `target` along the shortest chain of
    /// steps, rewriting `machine_version` to each hop's target and dropping `machine_hash` when
    /// at least one step ran. `machine_id` defaults to the blob's own `machine_id`. The blob is
    /// consumed; clone it first to keep the original.
    ///
    /// A step that rewrites `state_ids` but leaves `configuration` alone gets
    /// `configuration: null` (see the module notes); one that rewrote both inconsistently is
    /// refused.
    ///
    /// # Errors
    /// [`MigrationError::NoPath`] when no chain reaches `target`;
    /// [`MigrationError::StepFailed`] when a step errors, returns a non-object, or leaves
    /// `state_ids` naming states its `configuration` lacks.
    pub fn migrate(
        &self,
        blob: Value,
        target: Option<&str>,
        machine_id: Option<&str>,
    ) -> std::result::Result<Value, MigrationError> {
        let mid = machine_id
            .map(str::to_owned)
            .or_else(|| blob.get("machine_id").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_default();
        let found = match blob.get("machine_version") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(other) => Some(other.to_string()),
        };
        let hops = self.path_for(Some(&mid), found.as_deref(), target).ok_or_else(|| {
            MigrationError::NoPath {
                machine_id: mid.clone(),
                found: found.clone(),
                target: target.map(str::to_owned),
            }
        })?;
        let steps = self.applicable(Some(&mid));
        let mut out = blob;
        for (from, to) in &hops {
            let Some(entry) = steps.iter().find(|e| e.from == *from && e.to == *to) else {
                continue;
            };
            out = run_step(&entry.step, out, &mid, from, to)?;
            if let Some(obj) = out.as_object_mut() {
                obj.insert("machine_version".into(), Value::String(to.clone()));
            }
        }
        if !hops.is_empty()
            && let Some(obj) = out.as_object_mut()
        {
            obj.remove("machine_hash");
        }
        Ok(out)
    }
}

/// Apply one user step and normalise its result: whatever the step does wrong comes out as a
/// typed error naming the hop.
fn run_step(
    step: &MigrationStep,
    blob: Value,
    machine_id: &str,
    from: &str,
    to: &str,
) -> std::result::Result<Value, MigrationError> {
    let fail = |message: String| MigrationError::StepFailed {
        machine_id: machine_id.to_owned(),
        from: from.to_owned(),
        to: to.to_owned(),
        message,
    };
    let before_ids = blob.get("state_ids").cloned();
    let before_conf = blob.get("configuration").cloned();
    let mut out = step(blob).map_err(|e| fail(e.to_string()))?;
    let Some(obj) = out.as_object_mut() else {
        return Err(fail("must return a JSON object".into()));
    };
    let ids = obj.get("state_ids").cloned();
    let conf = obj.get("configuration").cloned();
    let (Some(Value::Array(ids)), conf @ (None | Some(Value::Array(_) | Value::Null))) =
        (ids, conf)
    else {
        return Ok(out); // a malformed field is reported by the restore itself
    };
    let untouched = conf == before_conf && Some(Value::Array(ids.clone())) != before_ids;
    let Some(Value::Array(conf)) = conf.filter(|_| !untouched) else {
        obj.insert("configuration".into(), Value::Null); // re-derived on restore
        return Ok(out);
    };
    let missing: Vec<String> = ids
        .iter()
        .filter(|id| !conf.contains(id))
        .map(|id| id.as_str().map_or_else(|| id.to_string(), str::to_owned))
        .collect();
    if missing.is_empty() {
        Ok(out)
    } else {
        Err(fail(format!(
            "left 'state_ids' naming {missing:?} which its 'configuration' does not contain"
        )))
    }
}
