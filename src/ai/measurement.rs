//! Explicitly owned measurement snapshots. Never discovers arbitrary snapshots.
//! The lock coordinates users of this API; uncooperative filesystem mutation is
//! outside this protocol. Interrupted record publication fails closed.
//! A failure removing a registration after its data was deleted preserves exact
//! deletion counts but requires manual inspection; reopening then fails closed.
//! Ordinary data deletion failures can be retried without losing dependencies.
use super::Snapshot;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

const MARKER: &str = ".willow-measurement.json";
const LOCK: &str = ".willow-measurement.lock";
const PENDING: &str = ".willow-measurement.pending";
const RECORD_PREFIX: &str = ".willow-measurement-record-";
const MAGIC: &str = "willow-owned-measurement-v2";
const MAX_RECORD: u64 = 64 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ownership {
    format: String,
}
#[derive(Default)]
struct Inventory {
    files: HashMap<String, Owned>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Owned {
    bytes: u64,
    sha256: String,
    base: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registration {
    name: String,
    owned: Owned,
}
fn record_name(name: &str) -> String {
    format!("{RECORD_PREFIX}{:x}", Sha256::digest(name.as_bytes()))
}
fn read_record<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    ensure!(
        regular(path)?.len() <= MAX_RECORD,
        "measurement record too large"
    );
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_RECORD + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_RECORD,
        "measurement record too large"
    );
    serde_json::from_slice(&bytes).context("invalid measurement record")
}

/// Keep this value alive for the entire measurement write session.
pub struct MeasurementStore {
    dir: PathBuf,
    _lock: File,
    inventory: Inventory,
    registration_sizes: HashMap<String, u64>,
    directory_entries: usize,
    dependency_edges: usize,
    verified_bytes: u64,
    poisoned: bool,
}

fn filename(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && !name.contains(['/', '\\', ':', '<', '>', '"', '|', '?', '*'])
            && !name.starts_with('.')
            && !name.ends_with(['.', ' '])
            && name.chars().all(|c| !c.is_control())
            && matches!(
                Path::new(name).components().next(),
                Some(Component::Normal(_))
            )
            && Path::new(name).components().count() == 1,
        "snapshot name must be a normal, non-hidden filename"
    );
    // Reject Windows device aliases on every host, including aliases with suffixes.
    let stem = name.split('.').next().unwrap().to_ascii_uppercase();
    ensure!(
        !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            && !(stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && matches!(stem.as_bytes()[3], b'1'..=b'9')),
        "reserved snapshot name"
    );
    Ok(())
}
fn regular(path: &Path) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.file_type().is_file(),
        "non-regular measurement file: {}",
        path.display()
    );
    Ok(metadata)
}
// Check each lexical prefix before canonicalization; canonicalization alone
// would silently authorize deletion through a symlinked parent directory.
fn scope_path(path: &Path, allow_absent: bool) -> Result<()> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut prefix = PathBuf::new();
    let components: Vec<_> = absolute.components().collect();
    for component in &components {
        prefix.push(component.as_os_str());
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        match fs::symlink_metadata(&prefix) {
            Ok(metadata) => ensure!(
                metadata.file_type().is_dir(),
                "measurement scope contains a non-directory or symlink: {}",
                prefix.display()
            ),
            Err(error) if allow_absent && error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
fn directory(path: &Path) -> Result<PathBuf> {
    scope_path(path, false)?;
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_dir(),
        "measurement scope must be a real directory"
    );
    Ok(fs::canonicalize(path)?)
}
fn digest(path: &Path) -> Result<(u64, String)> {
    regular(path)?;
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut bytes = 0;
    let mut buffer = [0; 65536];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes += read as u64;
    }
    Ok((bytes, format!("{:x}", hasher.finalize())))
}

impl MeasurementStore {
    /// Initialize only an absent or empty dedicated directory.
    pub fn create(dir: &Path) -> Result<Self> {
        scope_path(dir, true)?;
        match fs::symlink_metadata(dir) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(dir)?,
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        let dir = directory(dir)?;
        ensure!(
            fs::read_dir(&dir)?.next().is_none(),
            "measurement scope is not empty"
        );
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(dir.join(LOCK))?;
        lock.try_lock().context("measurement writer is active")?;
        let store = Self {
            dir,
            _lock: lock,
            inventory: Inventory::default(),
            registration_sizes: HashMap::new(),
            directory_entries: 1,
            dependency_edges: 0,
            verified_bytes: 0,
            poisoned: false,
        };
        // Recheck after exclusive lock creation so concurrent initialization cannot
        // turn a nonempty directory into a registered measurement scope.
        for entry in fs::read_dir(&store.dir)? {
            ensure!(
                entry?.file_name() == LOCK,
                "measurement scope changed during initialization"
            );
        }
        store.publish_record(
            MARKER,
            &Ownership {
                format: MAGIC.into(),
            },
        )?;
        Ok(store)
    }

    /// Validate constant-size ownership controls and acquire the writer lock.
    /// Existing snapshots are not read. Only clear/open authorize enumeration or
    /// deletion; save verifies an explicitly requested base on demand.
    pub fn open_writer(dir: &Path) -> Result<Self> {
        let dir = directory(dir)?;
        regular(&dir.join(LOCK))?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.join(LOCK))?;
        lock.try_lock().context("measurement writer is active")?;
        let ownership: Ownership = read_record(&dir.join(MARKER))?;
        ensure!(
            ownership.format == MAGIC,
            "unrecognized measurement ownership"
        );
        ensure!(
            fs::symlink_metadata(dir.join(PENDING))
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "interrupted measurement registration; inspect scope"
        );
        Ok(Self {
            dir,
            _lock: lock,
            inventory: Inventory::default(),
            registration_sizes: HashMap::new(),
            directory_entries: 0,
            dependency_edges: 0,
            verified_bytes: 0,
            poisoned: false,
        })
    }

    /// Fully validate exact data/registration pairs before inspection or clear.
    pub fn open(dir: &Path) -> Result<Self> {
        let mut store = Self::open_writer(dir)?;
        store.validate()?;
        Ok(store)
    }

    fn validate(&mut self) -> Result<()> {
        let mut data = HashMap::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            self.directory_entries += 1;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("non-UTF8 file in measurement scope"))?;
            ensure!(
                entry.file_type()?.is_file(),
                "non-regular file in measurement scope: {name}"
            );
            if matches!(name.as_str(), MARKER | LOCK) {
                continue;
            }
            if name.starts_with(RECORD_PREFIX) {
                let registration: Registration = read_record(&entry.path())?;
                filename(&registration.name)?;
                ensure!(
                    name == record_name(&registration.name),
                    "measurement registration name mismatch"
                );
                self.registration_sizes
                    .insert(registration.name.clone(), entry.metadata()?.len());
                ensure!(
                    self.inventory
                        .files
                        .insert(registration.name, registration.owned)
                        .is_none(),
                    "duplicate measurement registration"
                );
            } else {
                filename(&name)?;
                data.insert(name, entry.path());
            }
        }
        ensure!(
            data.len() == self.inventory.files.len(),
            "unregistered or missing measurement snapshot"
        );
        for (name, record) in &self.inventory.files {
            if let Some(base) = &record.base {
                self.dependency_edges += 1;
                filename(base)?;
                ensure!(
                    self.inventory
                        .files
                        .get(base)
                        .is_some_and(|r| r.base.is_none()),
                    "delta base must be an internally registered full snapshot"
                );
            }
            let path = data
                .get(name)
                .with_context(|| format!("registered snapshot is missing: {name}"))?;
            let (bytes, hash) = digest(path)?;
            self.verified_bytes += bytes;
            ensure!(
                bytes == record.bytes && hash == record.sha256,
                "registered snapshot changed: {name}"
            );
        }
        Ok(())
    }

    fn publish_record(&self, name: &str, value: &impl Serialize) -> Result<()> {
        self.publish_record_using(name, value, &|file, bytes| {
            file.write_all(bytes)?;
            file.sync_all()
        })
    }
    fn publish_record_using(
        &self,
        name: &str,
        value: &impl Serialize,
        write: &dyn Fn(&mut File, &[u8]) -> std::io::Result<()>,
    ) -> Result<()> {
        let bytes = serde_json::to_vec(value)?;
        ensure!(
            bytes.len() as u64 <= MAX_RECORD,
            "measurement record too large"
        );
        let pending = self.dir.join(PENDING);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&pending)?;
        let result = (|| -> Result<()> {
            write(&mut file, &bytes)?;
            drop(file);
            fs::hard_link(&pending, self.dir.join(name))
                .context("publish registration (destination must not exist)")?;
            fs::remove_file(&pending)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(pending);
        }
        result
    }

    fn full_base(&mut self, name: &str) -> Result<()> {
        filename(name)?;
        let registration: Registration = read_record(&self.dir.join(record_name(name)))
            .context("delta base must be internally registered")?;
        ensure!(
            registration.name == name && registration.owned.base.is_none(),
            "delta base must be an internally registered full snapshot"
        );
        let (bytes, hash) = digest(&self.dir.join(name))?;
        self.verified_bytes += bytes;
        self.dependency_edges += 1;
        ensure!(
            bytes == registration.owned.bytes && hash == registration.owned.sha256,
            "registered delta base changed"
        );
        Ok(())
    }

    /// Publish a compiler-validated snapshot and register its exact bytes.
    pub fn save(&mut self, name: &str, snapshot: &Snapshot, base: Option<&str>) -> Result<()> {
        ensure!(
            !self.poisoned,
            "measurement registration previously failed; reopen and inspect scope"
        );
        filename(name)?;
        ensure!(
            fs::symlink_metadata(self.dir.join(record_name(name)))
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "snapshot is already registered or registration cannot be inspected"
        );
        let path = self.dir.join(name);
        if let Some(base) = base {
            self.full_base(base)?;
            snapshot.save_delta(&path, &self.dir.join(base))?;
        } else {
            snapshot.save(&path)?;
        }
        let (bytes, sha256) = digest(&path)?;
        let registration = Registration {
            name: name.into(),
            owned: Owned {
                bytes,
                sha256,
                base: base.map(str::to_owned),
            },
        };
        if let Err(error) = self.publish_record(&record_name(name), &registration) {
            self.poisoned = true;
            // A failed registration leaves an unknown snapshot. Never continue
            // after that failure within this session.
            return Err(error
                .context("snapshot published but registration failed; scope requires inspection"));
        }
        self.inventory
            .files
            .insert(registration.name, registration.owned);
        Ok(())
    }
}

fn report(dir: &Path, dry_run: bool) -> Value {
    json!({"scope":dir.to_string_lossy(), "status":"ok", "success":true,
        "dry_run":dry_run, "deleted_count":0, "deleted_bytes":0,
        "deleted_registration_count":0, "deleted_registration_bytes":0,
        "planned_registration_count":0, "planned_registration_bytes":0,
        "planned_count":0, "planned_bytes":0, "skipped":[], "reasons":[],
        "directory_scans":0, "directory_entries":0, "dependency_edges":0,
        "verified_bytes":0, "os_page_cache":"unchanged"})
}
fn failed(value: &mut Value, error: impl std::fmt::Display) {
    value["status"] = json!("failed");
    value["success"] = json!(false);
    value["reasons"]
        .as_array_mut()
        .unwrap()
        .push(json!(error.to_string()));
}

/// Clear a complete, explicitly registered snapshot set; never recurse.
/// Validation/rejection and partial deletion failures are machine-readable.
pub fn clear(dir: &Path, dry_run: bool) -> Result<Value> {
    clear_using(dir, dry_run, &|path| fs::remove_file(path))
}
fn clear_using(
    dir: &Path,
    dry_run: bool,
    remove: &dyn Fn(&Path) -> std::io::Result<()>,
) -> Result<Value> {
    let mut result = report(dir, dry_run);
    if let Err(error) = scope_path(dir, true) {
        failed(&mut result, error);
        return Ok(result);
    }
    match fs::symlink_metadata(dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            result["skipped"] = json!(["absent"]);
            return Ok(result);
        }
        Err(error) => {
            failed(&mut result, error);
            return Ok(result);
        }
        Ok(_) => {}
    }
    let store = match MeasurementStore::open(dir) {
        Ok(store) => store,
        Err(error) => {
            failed(&mut result, format!("{error:#}"));
            return Ok(result);
        }
    };
    result["scope"] = json!(store.dir.to_string_lossy());
    result["directory_scans"] = json!(1);
    result["directory_entries"] = json!(store.directory_entries);
    result["dependency_edges"] = json!(store.dependency_edges);
    result["verified_bytes"] = json!(store.verified_bytes);
    result["planned_count"] = json!(store.inventory.files.len());
    result["planned_registration_count"] = json!(store.registration_sizes.len());
    result["planned_registration_bytes"] = json!(store.registration_sizes.values().sum::<u64>());
    result["planned_bytes"] = json!(store.inventory.files.values().map(|v| v.bytes).sum::<u64>());
    if dry_run {
        result["skipped"] = json!(["dry-run"]);
        return Ok(result);
    }
    if store.inventory.files.is_empty() {
        result["skipped"] = json!(["empty"]);
        return Ok(result);
    }
    // Delete dependent files first. On a failure every remaining dependency
    // still has its base. Two linear passes avoid sorting/repeated graph scans.
    let names: Vec<_> = [true, false]
        .into_iter()
        .flat_map(|delta| {
            store
                .inventory
                .files
                .iter()
                .filter(move |(_, v)| v.base.is_some() == delta)
                .map(|(k, v)| (k.clone(), v.bytes, store.registration_sizes[k]))
        })
        .collect();
    let mut count = 0;
    let mut bytes = 0;
    let mut registration_count = 0;
    let mut registration_bytes = 0;
    for (name, size, registration_size) in names {
        if let Err(error) = remove(&store.dir.join(&name)) {
            failed(&mut result, format!("delete {name}: {error}"));
            break;
        }
        count += 1;
        bytes += size;
        if let Err(error) = remove(&store.dir.join(record_name(&name))) {
            failed(
                &mut result,
                format!(
                    "registration removal failed after deleting {name}; scope requires inspection: {error}"
                ),
            );
            break;
        }
        registration_count += 1;
        registration_bytes += registration_size;
    }
    result["deleted_registration_count"] = json!(registration_count);
    result["deleted_registration_bytes"] = json!(registration_bytes);
    result["deleted_count"] = json!(count);
    result["deleted_bytes"] = json!(bytes);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Scope(PathBuf);
    impl Scope {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            Self(
                fs::canonicalize(std::env::temp_dir())
                    .unwrap()
                    .join(format!(
                        "willow-measurement-{}-{}",
                        std::process::id(),
                        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    )),
            )
        }
        fn create(&self) -> MeasurementStore {
            MeasurementStore::create(&self.0).unwrap()
        }
    }
    impl Drop for Scope {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn register(store: &mut MeasurementStore, name: &str, base: Option<&str>) {
        let path = store.dir.join(name);
        fs::write(&path, name).unwrap();
        let (bytes, sha256) = digest(&path).unwrap();
        store.inventory.files.insert(
            name.into(),
            Owned {
                bytes,
                sha256,
                base: base.map(str::to_owned),
            },
        );
        store
            .publish_record(
                &record_name(name),
                &Registration {
                    name: name.into(),
                    owned: store.inventory.files[name].clone(),
                },
            )
            .unwrap();
    }
    fn snapshot(scope: &Path) -> Snapshot {
        let mut snapshot = Snapshot {
            version: 1,
            compiler: super::super::storage::compiler_stamp(),
            compatibility: "measurement-test".into(),
            workspace: scope.to_string_lossy().into_owned(),
            revision: String::new(),
            sources: std::collections::BTreeMap::new(),
            functions: vec![],
            semantic: super::super::SemanticFacts::default(),
            edit_context: None,
        };
        snapshot.revision = snapshot.digest().unwrap();
        snapshot
    }
    #[test]
    fn validated_save_registration_and_internal_delta_roundtrip() {
        let s = Scope::new();
        let mut store = s.create();
        let snapshot = snapshot(&s.0);
        store.save("base.json", &snapshot, None).unwrap();
        assert!(store.save("base.json", &snapshot, None).is_err());
        assert!(
            store
                .save("outside.json", &snapshot, Some("../base.json"))
                .is_err()
        );
        assert!(
            store
                .save("missing.json", &snapshot, Some("missing-base.json"))
                .is_err()
        );
        store
            .save("delta.json", &snapshot, Some("base.json"))
            .unwrap();
        assert!(
            store
                .save("chain.json", &snapshot, Some("delta.json"))
                .is_err()
        );
        assert_eq!(
            Snapshot::load(&s.0.join("delta.json")).unwrap().revision,
            snapshot.revision
        );
        let mut corrupt = snapshot.clone();
        corrupt.revision = "wrong".into();
        assert!(store.save("invalid.json", &corrupt, None).is_err());
        assert!(!s.0.join("invalid.json").exists());
        drop(store);
        let result = clear(&s.0, false).unwrap();
        assert_eq!(result["success"], true);
        assert_eq!(result["deleted_count"], 2);
    }

    #[test]
    fn indexed_registrations_keep_reopened_writers_independent_of_history() {
        for n in [8, 16, 32, 64] {
            let scope = Scope::new();
            drop(scope.create());
            let snapshot = snapshot(&scope.0);
            let marker = fs::read(scope.0.join(MARKER)).unwrap();
            let mut record_bytes = None;
            for i in 0..n {
                let mut writer = MeasurementStore::open_writer(&scope.0).unwrap();
                assert_eq!(writer.directory_entries, 0);
                assert_eq!(writer.verified_bytes, 0);
                assert!(writer.inventory.files.is_empty());
                let name = format!("snapshot-{i:04}.json");
                writer.save(&name, &snapshot, None).unwrap();
                assert_eq!(writer.directory_entries, 0);
                assert_eq!(
                    writer.verified_bytes, 0,
                    "full saves read no prior payloads"
                );
                let size = fs::metadata(scope.0.join(record_name(&name)))
                    .unwrap()
                    .len();
                assert_eq!(*record_bytes.get_or_insert(size), size);
            }
            assert_eq!(fs::read(scope.0.join(MARKER)).unwrap(), marker);
            let plan = clear(&scope.0, true).unwrap();
            assert_eq!(plan["directory_scans"], 1);
            assert_eq!(plan["directory_entries"], 2 * n + 2);
            assert_eq!(plan["planned_count"], n);
            assert_eq!(clear(&scope.0, false).unwrap()["deleted_count"], n);
            assert_eq!(fs::read_dir(&scope.0).unwrap().count(), 2);
        }
    }

    #[test]
    fn fast_writer_verifies_only_requested_base_and_clear_verifies_everything() {
        let scope = Scope::new();
        let snapshot = snapshot(&scope.0);
        let mut store = scope.create();
        store.save("base.json", &snapshot, None).unwrap();
        drop(store);
        fs::write(scope.0.join("base.json"), "changed").unwrap();
        let mut writer = MeasurementStore::open_writer(&scope.0).unwrap();
        writer.save("independent.json", &snapshot, None).unwrap();
        assert_eq!(writer.verified_bytes, 0);
        assert!(
            writer
                .save("delta.json", &snapshot, Some("base.json"))
                .is_err()
        );
        assert_eq!(writer.verified_bytes, 7);
        assert!(!scope.0.join("delta.json").exists());
        drop(writer);
        let result = clear(&scope.0, false).unwrap();
        assert_eq!(result["success"], false);
        assert_eq!(result["deleted_count"], 0);
        assert!(scope.0.join("independent.json").exists());
    }

    #[test]
    fn malformed_misnamed_oversized_and_unknown_record_fields_fail_closed() {
        for defect in ["malformed", "misnamed", "oversized", "unknown"] {
            let scope = Scope::new();
            let mut store = scope.create();
            register(&mut store, "base.json", None);
            let path = scope.0.join(record_name("base.json"));
            match defect {
                "malformed" => fs::write(&path, "{").unwrap(),
                "misnamed" => fs::rename(&path, scope.0.join(record_name("other.json"))).unwrap(),
                "oversized" => OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_len(MAX_RECORD + 1)
                    .unwrap(),
                "unknown" => {
                    let mut value: Value =
                        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                    value["unexpected"] = json!(true);
                    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
                }
                _ => unreachable!(),
            }
            drop(store);
            let result = clear(&scope.0, false).unwrap();
            assert_eq!(result["success"], false, "{defect}");
            assert_eq!(result["deleted_count"], 0);
            assert!(scope.0.join("base.json").exists());
        }
    }

    #[test]
    fn interrupted_record_publication_never_authorizes_unregistered_data() {
        let scope = Scope::new();
        let store = scope.create();
        fs::write(scope.0.join("base.json"), "base.json").unwrap();
        let (bytes, sha256) = digest(&scope.0.join("base.json")).unwrap();
        let record = Registration {
            name: "base.json".into(),
            owned: Owned {
                bytes,
                sha256,
                base: None,
            },
        };
        assert!(
            store
                .publish_record_using(&record_name("base.json"), &record, &|file, bytes| {
                    file.write_all(&bytes[..5])?;
                    Err(std::io::Error::other("injected registration write failure"))
                })
                .is_err()
        );
        assert!(!scope.0.join(record_name("base.json")).exists());
        drop(store);
        assert_eq!(clear(&scope.0, false).unwrap()["success"], false);
        assert!(scope.0.join("base.json").exists());
    }

    #[test]
    fn registration_deletion_failure_preserves_data_counts_and_requires_inspection() {
        let scope = Scope::new();
        let mut store = scope.create();
        register(&mut store, "base.json", None);
        register(&mut store, "delta.json", Some("base.json"));
        drop(store);
        let result = clear_using(&scope.0, false, &|path| {
            if path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(RECORD_PREFIX)
            {
                Err(std::io::Error::other("injected metadata deletion failure"))
            } else {
                fs::remove_file(path)
            }
        })
        .unwrap();
        assert_eq!(result["success"], false);
        assert_eq!(result["deleted_count"], 1);
        assert_eq!(result["deleted_bytes"], 10);
        assert_eq!(result["deleted_registration_count"], 0);
        assert_eq!(result["deleted_registration_bytes"], 0);
        assert!(!scope.0.join("delta.json").exists());
        assert!(scope.0.join("base.json").exists());
        assert!(scope.0.join(record_name("delta.json")).exists());
        assert!(MeasurementStore::open(&scope.0).is_err());
        assert_eq!(clear(&scope.0, false).unwrap()["deleted_count"], 0);
    }

    #[test]
    fn absent_empty_repeated_and_active_writer() {
        let s = Scope::new();
        assert_eq!(clear(&s.0, false).unwrap()["skipped"], json!(["absent"]));
        let store = s.create();
        assert!(!clear(&s.0, false).unwrap()["success"].as_bool().unwrap());
        assert!(MeasurementStore::open(&s.0).is_err());
        drop(store);
        for _ in 0..2 {
            assert_eq!(clear(&s.0, false).unwrap()["skipped"], json!(["empty"]));
        }
        assert!(s.0.join(MARKER).is_file());
    }
    #[test]
    fn exact_owned_set_dry_run_and_dependencies() {
        let s = Scope::new();
        let mut store = s.create();
        register(&mut store, "base.json", None);
        register(&mut store, "delta.json", Some("base.json"));
        drop(store);
        let planned = clear(&s.0, true).unwrap();
        assert_eq!(planned["planned_count"], 2);
        assert_eq!(planned["deleted_count"], 0);
        assert_eq!(planned["directory_entries"], 6);
        assert_eq!(planned["dependency_edges"], 1);
        let done = clear(&s.0, false).unwrap();
        assert_eq!(done["deleted_count"], 2);
        assert_eq!(done["deleted_registration_count"], 2);
        assert_eq!(
            done["deleted_registration_bytes"],
            planned["planned_registration_bytes"]
        );
        assert_eq!(done["deleted_bytes"], 19);
        assert_eq!(done["success"], true);
        assert_eq!(clear(&s.0, false).unwrap()["planned_count"], 0);
    }
    #[test]
    fn unknown_changed_missing_and_malformed_fail_closed() {
        for failure in [
            "unknown",
            "changed",
            "missing",
            "malformed",
            "external",
            "chain",
        ] {
            let s = Scope::new();
            let mut store = s.create();
            register(&mut store, "base.json", None);
            match failure {
                "unknown" => fs::write(s.0.join("recovery-journal"), "keep").unwrap(),
                "changed" => fs::write(s.0.join("base.json"), "changed").unwrap(),
                "missing" => fs::remove_file(s.0.join("base.json")).unwrap(),
                "malformed" => fs::write(s.0.join(MARKER), "{}").unwrap(),
                "external" => {
                    register(&mut store, "delta.json", Some("../outside"));
                }
                "chain" => {
                    register(&mut store, "delta.json", Some("base.json"));
                    register(&mut store, "chain.json", Some("delta.json"));
                }
                _ => unreachable!(),
            }
            drop(store);
            let value = clear(&s.0, false).unwrap();
            assert_eq!(value["success"], false, "{failure}");
            assert_eq!(value["deleted_count"], 0);
            if failure != "missing" {
                assert!(s.0.join("base.json").exists());
            }
        }
    }
    #[test]
    fn names_and_nonempty_scope_are_rejected() {
        for name in [
            "", ".", "..", "../x", "a/b", "a\\b", "C:x", MARKER, LOCK, PENDING, "CON", "nul.json",
            "COM1", "x.", "x ",
        ] {
            assert!(filename(name).is_err(), "{name}");
        }
        for name in ["full.json", "delta-1.json", "日本語.json"] {
            filename(name).unwrap();
        }
        let s = Scope::new();
        fs::create_dir(&s.0).unwrap();
        fs::write(s.0.join("source.wi"), "keep").unwrap();
        assert!(MeasurementStore::create(&s.0).is_err());
        assert_eq!(fs::read_to_string(s.0.join("source.wi")).unwrap(), "keep");
    }
    #[test]
    fn partial_failure_updates_inventory_without_dangling_base() {
        let s = Scope::new();
        let mut store = s.create();
        register(&mut store, "base.json", None);
        register(&mut store, "a.json", Some("base.json"));
        register(&mut store, "b.json", Some("base.json"));
        drop(store);
        let data_calls = std::cell::Cell::new(0);
        let value = clear_using(&s.0, false, &|path| {
            if !path.file_name().unwrap().to_string_lossy().starts_with('.') {
                data_calls.set(data_calls.get() + 1);
            }
            if data_calls.get() == 2 {
                Err(std::io::Error::other("injected deletion failure"))
            } else {
                fs::remove_file(path)
            }
        })
        .unwrap();
        assert_eq!(value["success"], false);
        assert_eq!(value["deleted_count"], 1);
        let remaining = MeasurementStore::open(&s.0).unwrap();
        assert_eq!(remaining.inventory.files.len(), 2);
        assert!(remaining.inventory.files.contains_key("base.json"));
        drop(remaining);
        assert_eq!(clear(&s.0, false).unwrap()["deleted_count"], 2);
    }
    #[test]
    fn scan_counts_scale_with_files_and_edges() {
        for n in [8, 16, 32, 64] {
            let s = Scope::new();
            let mut store = s.create();
            register(&mut store, "base.json", None);
            for i in 0..n {
                register(&mut store, &format!("delta-{i}.json"), Some("base.json"));
            }
            drop(store);
            let value = clear(&s.0, true).unwrap();
            assert_eq!(value["directory_scans"], 1);
            assert_eq!(value["directory_entries"], 2 * n + 4);
            assert_eq!(value["dependency_edges"], n);
            println!(
                "measurement-clear payloads={} scans={} entries={} edges={} verified_bytes={}",
                n + 1,
                value["directory_scans"],
                value["directory_entries"],
                value["dependency_edges"],
                value["verified_bytes"]
            );
        }
    }
    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_scope_ancestors_before_creation_or_deletion() {
        use std::os::unix::fs::symlink;
        let real = Scope::new();
        fs::create_dir(&real.0).unwrap();
        let nested = real.0.join("nested");
        let mut store = MeasurementStore::create(&nested).unwrap();
        register(&mut store, "base.json", None);
        drop(store);
        let link = Scope::new();
        symlink(&real.0, &link.0).unwrap();
        assert_eq!(
            clear(&link.0.join("nested"), false).unwrap()["success"],
            false
        );
        assert!(nested.join("base.json").is_file());
        assert!(MeasurementStore::create(&link.0.join("new")).is_err());
        assert!(!real.0.join("new").exists());
        fs::remove_file(&link.0).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rejects_scope_entry_and_control_symlinks() {
        use std::os::unix::fs::symlink;
        for name in [
            "base.json".to_owned(),
            LOCK.into(),
            MARKER.into(),
            "unknown".into(),
            record_name("base.json"),
            PENDING.into(),
        ] {
            let s = Scope::new();
            let mut store = s.create();
            register(&mut store, "base.json", None);
            drop(store);
            let outside = Scope::new();
            fs::write(&outside.0, "keep").unwrap();
            if s.0.join(&name).exists() {
                fs::remove_file(s.0.join(&name)).unwrap();
            }
            symlink(&outside.0, s.0.join(&name)).unwrap();
            assert_eq!(clear(&s.0, false).unwrap()["success"], false);
            assert_eq!(fs::read_to_string(&outside.0).unwrap(), "keep");
            fs::remove_file(&outside.0).unwrap();
        }
        let real = Scope::new();
        drop(real.create());
        let link = Scope::new();
        symlink(&real.0, &link.0).unwrap();
        assert_eq!(clear(&link.0, false).unwrap()["success"], false);
        fs::remove_file(&link.0).unwrap();
    }
}
