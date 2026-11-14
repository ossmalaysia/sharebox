//! Owner-scoped, bounded, crash-recoverable snapshot staging. Network handlers
//! supply authenticated stable principal IDs; bearer credentials never enter here.
use serde::{Deserialize, Serialize};
use sharebox::snapshot::{
    hash_bytes, validate_manifest, SnapshotManifest, CHUNK_BYTES, MAX_MANIFEST_BYTES,
};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

type Result<T> = std::result::Result<T, String>;
const QUOTA: u64 = 2 * 1024 * 1024 * 1024;
const TTL: u64 = 30 * 60 * 1000;
const MAX_RECORDS: usize = 256;
const MAX_INDEX: usize = 512 * 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Upload {
    id: String,
    owner: String,
    snapshot_id: String,
    created_at: u64,
    reserved: u64,
    file_count: usize,
    total_bytes: u64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Snapshot {
    id: String,
    owner: String,
    storage_id: String,
    upload_ids: Vec<String>,
    created_at: u64,
    reserved: u64,
    file_count: usize,
    total_bytes: u64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Garbage {
    id: String,
    reserved: u64,
    file_count: usize,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    uploads: Vec<Upload>,
    snapshots: Vec<Snapshot>,
    garbage: Vec<Garbage>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub snapshot_id: String,
    pub file_count: usize,
    pub total_bytes: u64,
}
impl Snapshot {
    fn summary(&self) -> Summary {
        Summary {
            snapshot_id: self.id.clone(),
            file_count: self.file_count,
            total_bytes: self.total_bytes,
        }
    }
}
pub struct SnapshotStore {
    root: PathBuf,
}

fn authorized(requester: &str, owner: &str) -> bool {
    requester == "admin" || requester == owner
}
fn io_error(_: std::io::Error) -> String {
    "Snapshot storage I/O failed; inspect the master's data directory".into()
}
pub fn valid_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn random_id() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|_| "Could not generate snapshot upload ID")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
fn read_bounded(path: &Path, maximum: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(io_error)?
        .take(maximum as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() > maximum {
        return Err("Snapshot metadata exceeds its size limit".into());
    }
    Ok(bytes)
}
fn sync_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(io_error)?;
    #[cfg(not(unix))]
    let _ = path; // Windows replacement uses MOVEFILE_WRITE_THROUGH.
    Ok(())
}
fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(io_error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(io_error)?;
    }
    Ok(())
}
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension("tmp");
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&temporary).map_err(io_error)?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(io_error)?;
    drop(file);
    crate::worker::replace_file(&temporary, path).map_err(io_error)?;
    sync_dir(path.parent().ok_or("Invalid snapshot storage path")?)
}

impl SnapshotStore {
    pub fn open(root: PathBuf) -> Result<Self> {
        private_dir(&root)?;
        private_dir(&root.join("data"))?;
        let store = Self { root };
        if !store.root.join("index.json").exists() {
            if fs::read_dir(store.root.join("data"))
                .map_err(io_error)?
                .next()
                .is_some()
            {
                return Err(
                    "Snapshot index is missing but stored data exists; refusing to overwrite it"
                        .into(),
                );
            }
            store.save(&Index::default())?;
        }
        store.load()?;
        Ok(store)
    }
    fn load(&self) -> Result<Index> {
        let index: Index =
            serde_json::from_slice(&read_bounded(&self.root.join("index.json"), MAX_INDEX)?)
                .map_err(|_| "Snapshot index is invalid; refusing to overwrite it")?;
        if index.uploads.len()
            + index.garbage.len()
            + index
                .snapshots
                .iter()
                .map(|s| s.upload_ids.len())
                .sum::<usize>()
            > MAX_RECORDS * 2
        {
            return Err("Snapshot index exceeds its record limit".into());
        }
        for (id, count) in index
            .uploads
            .iter()
            .map(|u| (&u.id, u.file_count))
            .chain(
                index
                    .snapshots
                    .iter()
                    .map(|s| (&s.storage_id, s.file_count)),
            )
            .chain(index.garbage.iter().map(|g| (&g.id, g.file_count)))
        {
            if !valid_id(id) || count > sharebox::snapshot::MAX_FILES {
                return Err("Snapshot index contains invalid storage metadata".into());
            }
        }
        Ok(index)
    }
    fn save(&self, index: &Index) -> Result<()> {
        let bytes = serde_json::to_vec(index).map_err(|_| "Cannot encode snapshot index")?;
        if bytes.len() > MAX_INDEX {
            return Err("Snapshot index size limit reached; delete unused snapshots".into());
        }
        atomic_write(&self.root.join("index.json"), &bytes)
    }
    fn storage(&self, id: &str) -> Result<PathBuf> {
        if !valid_id(id) {
            return Err("Invalid snapshot storage identifier".into());
        }
        Ok(self.root.join("data").join(id))
    }
    fn delete_storage(&self, garbage: &Garbage) -> Result<()> {
        let dir = self.storage(&garbage.id)?;
        // Only known flat filenames; never recursive or unbounded directory scans.
        for name in (0..garbage.file_count)
            .map(|i| format!("{i}.bin"))
            .chain(["manifest.json".into(), "manifest.tmp".into()])
        {
            match fs::remove_file(dir.join(name)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(io_error(error)),
            }
        }
        match fs::remove_dir(&dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(error)),
        }
        sync_dir(&self.root.join("data"))
    }
    fn prepare(&self, now: u64) -> Result<Index> {
        let mut index = self.load()?;
        let mut changed = false;
        let mut retained = Vec::new();
        for upload in index.uploads.drain(..) {
            if now.saturating_sub(upload.created_at) >= TTL {
                index.garbage.push(Garbage {
                    id: upload.id,
                    reserved: upload.reserved,
                    file_count: upload.file_count,
                });
                changed = true;
            } else {
                retained.push(upload);
            }
        }
        index.uploads = retained;
        if changed {
            self.save(&index)?;
        }
        // Garbage keeps its reservation until its files are durably removed.
        for _ in 0..4 {
            let Some(garbage) = index.garbage.first() else {
                break;
            };
            self.delete_storage(garbage)?;
            index.garbage.remove(0);
            self.save(&index)?;
        }
        Ok(index)
    }
    fn manifest(&self, storage: &str, expected: &str) -> Result<SnapshotManifest> {
        let bytes = read_bounded(
            &self.storage(storage)?.join("manifest.json"),
            MAX_MANIFEST_BYTES,
        )?;
        let manifest: SnapshotManifest =
            serde_json::from_slice(&bytes).map_err(|_| "Stored snapshot manifest is invalid")?;
        if hash_bytes(&validate_manifest(&manifest)?) != expected {
            return Err("Stored snapshot manifest hash mismatch".into());
        }
        Ok(manifest)
    }
    pub fn begin(&mut self, owner: &str, manifest: &SnapshotManifest, now: u64) -> Result<String> {
        let canonical = validate_manifest(manifest)?;
        let mut index = self.prepare(now)?;
        if index.uploads.iter().filter(|u| u.owner == owner).count() >= 2 {
            return Err("At most two active snapshot uploads are allowed per principal; retry after completion or expiry".into());
        }
        if index.uploads.len()
            + index.garbage.len()
            + index
                .snapshots
                .iter()
                .map(|s| s.upload_ids.len())
                .sum::<usize>()
            >= MAX_RECORDS
        {
            return Err("Snapshot metadata limit reached; delete unused snapshots".into());
        }
        // Reserve file allocation blocks and metadata, not just logical byte sizes.
        let reserved = manifest
            .files
            .iter()
            .map(|f| f.size.div_ceil(4096) * 4096)
            .sum::<u64>()
            + canonical.len() as u64
            + 8192;
        let used = index
            .uploads
            .iter()
            .map(|u| u.reserved)
            .chain(index.snapshots.iter().map(|s| s.reserved))
            .chain(index.garbage.iter().map(|g| g.reserved))
            .try_fold(0u64, |sum, bytes| sum.checked_add(bytes))
            .ok_or("Snapshot quota accounting overflow")?;
        if used.checked_add(reserved).is_none_or(|bytes| bytes > QUOTA) {
            return Err("Snapshot storage quota reached (2 GiB including reserved uploads); delete unused snapshots".into());
        }
        let id = random_id()?;
        index.uploads.push(Upload {
            id: id.clone(),
            owner: owner.into(),
            snapshot_id: hash_bytes(&canonical),
            created_at: now,
            reserved,
            file_count: manifest.files.len(),
            total_bytes: manifest.files.iter().map(|f| f.size).sum(),
        });
        // Persist reservation first, so a crash never leaves unaccounted staging data.
        self.save(&index)?;
        let dir = self.storage(&id)?;
        fs::create_dir(&dir).map_err(io_error)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).map_err(io_error)?;
        }
        sync_dir(&self.root.join("data"))?;
        atomic_write(&dir.join("manifest.json"), &canonical)?;
        Ok(id)
    }
    pub fn chunk(
        &mut self,
        owner: &str,
        upload_id: &str,
        file_index: usize,
        chunk_index: usize,
        bytes: &[u8],
        now: u64,
    ) -> Result<bool> {
        let index = self.prepare(now)?;
        let upload = index
            .uploads
            .iter()
            .find(|u| u.id == upload_id && authorized(owner, &u.owner))
            .ok_or("Snapshot upload not found or expired")?;
        let manifest = self.manifest(&upload.id, &upload.snapshot_id)?;
        let description = manifest
            .files
            .get(file_index)
            .ok_or("Snapshot file index out of range")?;
        let (offset, expected) = chunk_range(description.size, chunk_index)?;
        if bytes.len() != expected {
            return Err("Snapshot chunk length does not match the manifest".into());
        }
        let dir = self.storage(&upload.id)?;
        let path = dir.join(format!("{file_index}.bin"));
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(&path).map_err(io_error)?;
        let length = file.metadata().map_err(io_error)?.len();
        if length < offset || length > description.size {
            return Err("Snapshot chunks must be uploaded in order".into());
        }
        file.seek(SeekFrom::Start(offset)).map_err(io_error)?;
        let overlap = (length - offset).min(bytes.len() as u64) as usize;
        if overlap > 0 {
            let mut previous = vec![0; overlap];
            file.read_exact(&mut previous).map_err(io_error)?;
            if previous != bytes[..overlap] {
                return Err("Conflicting retry for an already uploaded snapshot chunk".into());
            }
        }
        let duplicate = overlap == bytes.len();
        if !duplicate {
            // Resume an interrupted partial write only after checking its prefix.
            file.write_all(&bytes[overlap..]).map_err(io_error)?;
        }
        file.sync_all().map_err(io_error)?;
        sync_dir(&dir)?;
        Ok(duplicate)
    }
    pub fn commit(&mut self, owner: &str, upload_id: &str, now: u64) -> Result<Summary> {
        let mut index = self.prepare(now)?;
        if let Some(snapshot) = index
            .snapshots
            .iter()
            .find(|s| authorized(owner, &s.owner) && s.upload_ids.iter().any(|id| id == upload_id))
        {
            return Ok(snapshot.summary());
        }
        let position = index
            .uploads
            .iter()
            .position(|u| u.id == upload_id && authorized(owner, &u.owner))
            .ok_or("Snapshot upload not found or expired")?;
        let upload = index.uploads[position].clone();
        let manifest = self.manifest(&upload.id, &upload.snapshot_id)?;
        for (i, description) in manifest.files.iter().enumerate() {
            let bytes = if description.size == 0 {
                Vec::new()
            } else {
                read_bounded(
                    &self.storage(&upload.id)?.join(format!("{i}.bin")),
                    description.size as usize,
                )?
            };
            if sharebox::snapshot::contains_worker_credentials(&bytes) {
                return Err(
                    "Snapshot contains a paired worker credential file; remove it before uploading"
                        .into(),
                );
            }
            if bytes.len() as u64 != description.size || hash_bytes(&bytes) != description.sha256 {
                return Err(
                    "Snapshot is incomplete or a file hash does not match its manifest".into(),
                );
            }
        }
        let summary;
        if let Some(existing) = index
            .snapshots
            .iter_mut()
            .find(|s| s.owner == upload.owner && s.id == upload.snapshot_id)
        {
            existing.upload_ids.push(upload.id.clone());
            summary = existing.summary();
            index.garbage.push(Garbage {
                id: upload.id,
                reserved: upload.reserved,
                file_count: upload.file_count,
            });
        } else {
            let snapshot = Snapshot {
                id: upload.snapshot_id,
                owner: upload.owner.clone(),
                storage_id: upload.id.clone(),
                upload_ids: vec![upload.id],
                created_at: now,
                reserved: upload.reserved,
                file_count: upload.file_count,
                total_bytes: upload.total_bytes,
            };
            summary = snapshot.summary();
            index.snapshots.push(snapshot);
        }
        index.uploads.remove(position);
        self.save(&index)?;
        Ok(summary)
    }
    pub fn list(&mut self, owner: &str, now: u64) -> Result<Vec<Summary>> {
        let index = self.prepare(now)?;
        let mut summaries = std::collections::BTreeMap::new();
        for snapshot in index
            .snapshots
            .iter()
            .filter(|s| authorized(owner, &s.owner))
        {
            summaries
                .entry(snapshot.id.clone())
                .or_insert_with(|| snapshot.summary());
        }
        Ok(summaries.into_values().collect())
    }
    pub fn resolve_owner(&mut self, requester: &str, id: &str, now: u64) -> Result<String> {
        let index = self.prepare(now)?;
        index
            .snapshots
            .iter()
            .find(|s| authorized(requester, &s.owner) && s.id == id)
            .map(|s| s.owner.clone())
            .ok_or("Snapshot not found for this principal".into())
    }
    pub fn owned_manifest(&mut self, owner: &str, id: &str, now: u64) -> Result<SnapshotManifest> {
        let index = self.prepare(now)?;
        let snapshot = index
            .snapshots
            .iter()
            .find(|s| authorized(owner, &s.owner) && s.id == id)
            .ok_or("Snapshot not found for this principal")?;
        self.manifest(&snapshot.storage_id, &snapshot.id)
    }
    pub fn read_chunk(
        &mut self,
        owner: &str,
        id: &str,
        file_index: usize,
        chunk_index: usize,
        now: u64,
    ) -> Result<Vec<u8>> {
        let index = self.prepare(now)?;
        let snapshot = index
            .snapshots
            .iter()
            .find(|s| authorized(owner, &s.owner) && s.id == id)
            .ok_or("Snapshot not found for this principal")?;
        let manifest = self.manifest(&snapshot.storage_id, &snapshot.id)?;
        let file = manifest
            .files
            .get(file_index)
            .ok_or("Snapshot file index out of range")?;
        let (offset, length) = chunk_range(file.size, chunk_index)?;
        let mut input = File::open(
            self.storage(&snapshot.storage_id)?
                .join(format!("{file_index}.bin")),
        )
        .map_err(io_error)?;
        input.seek(SeekFrom::Start(offset)).map_err(io_error)?;
        let mut bytes = vec![0; length];
        input.read_exact(&mut bytes).map_err(io_error)?;
        Ok(bytes)
    }
    pub fn delete(&mut self, owner: &str, id: &str, now: u64) -> Result<()> {
        let mut index = self.prepare(now)?;
        let mut retained = Vec::new();
        let mut found = false;
        for snapshot in index.snapshots.drain(..) {
            if authorized(owner, &snapshot.owner) && snapshot.id == id {
                found = true;
                index.garbage.push(Garbage {
                    id: snapshot.storage_id,
                    reserved: snapshot.reserved,
                    file_count: snapshot.file_count,
                });
            } else {
                retained.push(snapshot);
            }
        }
        if !found {
            return Err("Snapshot not found for this principal".into());
        }
        index.snapshots = retained;
        self.save(&index)?;
        self.prepare(now)?;
        Ok(())
    }
}
fn chunk_range(size: u64, index: usize) -> Result<(u64, usize)> {
    let offset = (index as u64)
        .checked_mul(CHUNK_BYTES as u64)
        .ok_or("Snapshot chunk index out of range")?;
    if offset >= size {
        return Err("Snapshot chunk index out of range; empty files have no chunks".into());
    }
    Ok((offset, (size - offset).min(CHUNK_BYTES as u64) as usize))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sharebox::snapshot::SnapshotFile;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> (Self, SnapshotStore) {
            let path =
                std::env::temp_dir().join(format!("sharebox-store-test-{}", random_id().unwrap()));
            let store = SnapshotStore::open(path.clone()).unwrap();
            (Self(path), store)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn manifest(bytes: &[u8]) -> SnapshotManifest {
        SnapshotManifest {
            version: 1,
            files: vec![SnapshotFile {
                path: "src/test.js".into(),
                size: bytes.len() as u64,
                sha256: hash_bytes(bytes),
                executable: false,
            }],
        }
    }
    #[test]
    fn durable_chunks_commit_retries_and_owner_isolation() {
        let (fixture, mut store) = Fixture::new();
        let bytes = vec![42; CHUNK_BYTES + 3];
        let upload = store.begin("token-a", &manifest(&bytes), 10).unwrap();
        assert!(store
            .chunk("token-b", &upload, 0, 0, &bytes[..CHUNK_BYTES], 11)
            .is_err());
        assert!(store
            .chunk("token-a", &upload, 0, 1, &bytes[CHUNK_BYTES..], 11)
            .is_err());
        assert!(!store
            .chunk("token-a", &upload, 0, 0, &bytes[..CHUNK_BYTES], 11)
            .unwrap());
        let mut store = SnapshotStore::open(fixture.0.clone()).unwrap();
        assert!(store
            .chunk("token-a", &upload, 0, 0, &bytes[..CHUNK_BYTES], 12)
            .unwrap());
        assert!(store
            .chunk("token-a", &upload, 0, 0, &vec![0; CHUNK_BYTES], 12)
            .is_err());
        assert!(store.commit("token-a", &upload, 12).is_err());
        store
            .chunk("token-a", &upload, 0, 1, &bytes[CHUNK_BYTES..], 12)
            .unwrap();
        let committed = store.commit("token-a", &upload, 12).unwrap();
        assert_eq!(
            store.commit("token-a", &upload, 13).unwrap().snapshot_id,
            committed.snapshot_id
        );
        assert!(store
            .owned_manifest("token-b", &committed.snapshot_id, 13)
            .is_err());
        assert!(store.list("token-b", 13).unwrap().is_empty());
        assert_eq!(
            store
                .read_chunk("token-a", &committed.snapshot_id, 0, 1, 13)
                .unwrap(),
            bytes[CHUNK_BYTES..]
        );
        store.delete("token-a", &committed.snapshot_id, 13).unwrap();
        assert!(store
            .owned_manifest("token-a", &committed.snapshot_id, 13)
            .is_err());
    }
    #[test]
    fn expiry_upload_limits_and_hash_verification() {
        let (_fixture, mut store) = Fixture::new();
        let manifest = manifest(b"abc");
        let upload = store.begin("a", &manifest, 0).unwrap();
        store.begin("a", &manifest, 0).unwrap();
        assert!(store.begin("a", &manifest, 0).is_err());
        store.chunk("a", &upload, 0, 0, b"bad", 1).unwrap();
        assert!(store.commit("a", &upload, 1).is_err());
        assert!(store.chunk("a", &upload, 0, 0, b"abc", 1).is_err());
        assert!(store.commit("a", &upload, TTL).is_err());
        assert!(store.begin("a", &manifest, TTL).is_ok());
    }
    #[test]
    fn empty_files_and_identical_uploads_are_deduplicated_with_retry_receipts() {
        let (_fixture, mut store) = Fixture::new();
        let manifest = manifest(b"");
        let first = store.begin("a", &manifest, 0).unwrap();
        let second = store.begin("a", &manifest, 0).unwrap();
        let committed = store.commit("a", &first, 1).unwrap();
        assert!(store.chunk("a", &second, 0, 0, b"", 1).is_err());
        assert_eq!(
            store.commit("a", &second, 1).unwrap().snapshot_id,
            committed.snapshot_id
        );
        assert_eq!(
            store.commit("a", &second, 2).unwrap().snapshot_id,
            committed.snapshot_id
        );
        assert_eq!(store.list("a", 2).unwrap().len(), 1);
        assert_eq!(fs::read_dir(store.root.join("data")).unwrap().count(), 1);
    }
    #[test]
    fn dedup_retry_receipts_stay_bounded_across_restart_and_release_on_delete() {
        let (fixture, mut store) = Fixture::new();
        let source = manifest(b"");
        let first = store.begin("a", &source, 0).unwrap();
        let snapshot = store.commit("a", &first, 0).unwrap().snapshot_id;
        // Seed a valid persisted receipt history near the cap; exercise the last
        // two actual uploads, commit recovery, refusal, and reclamation below.
        let mut index = store.load().unwrap();
        let original_reservation = index.snapshots[0].reserved;
        for number in 0..MAX_RECORDS - 3 {
            index.snapshots[0].upload_ids.push(format!("{number:064x}"));
        }
        store.save(&index).unwrap();
        let second = store.begin("a", &source, 0).unwrap();
        let third = store.begin("a", &source, 0).unwrap();
        store.commit("a", &second, 1).unwrap();
        let mut store = SnapshotStore::open(fixture.0.clone()).unwrap();
        assert_eq!(store.commit("a", &third, 1).unwrap().snapshot_id, snapshot);
        assert!(store
            .begin("b", &source, 2)
            .unwrap_err()
            .contains("metadata limit"));
        for upload in [&first, &second, &third] {
            assert_eq!(store.commit("a", upload, 2).unwrap().snapshot_id, snapshot);
        }
        let index = store.load().unwrap();
        assert_eq!(index.snapshots.len(), 1);
        assert_eq!(index.snapshots[0].upload_ids.len(), MAX_RECORDS);
        assert_eq!(index.snapshots[0].reserved, original_reservation);
        assert!(index.uploads.is_empty() && index.garbage.is_empty());
        assert_eq!(fs::read_dir(store.root.join("data")).unwrap().count(), 1);
        store.delete("a", &snapshot, 3).unwrap();
        assert!(store.begin("b", &source, 3).is_ok());
    }

    #[test]
    fn admin_can_manage_all_grants_without_making_hashes_public() {
        let (_fixture, mut store) = Fixture::new();
        let first = store.begin("a", &manifest(b""), 0).unwrap();
        let id = store.commit("a", &first, 0).unwrap().snapshot_id;
        assert!(store.owned_manifest("b", &id, 0).is_err());
        assert_eq!(store.resolve_owner("admin", &id, 0).unwrap(), "a");
        let second = store.begin("b", &manifest(b""), 0).unwrap();
        assert_eq!(store.commit("admin", &second, 0).unwrap().snapshot_id, id);
        assert!(store.owned_manifest("b", &id, 0).is_ok());
        assert_eq!(store.list("admin", 0).unwrap().len(), 1);
        store.delete("admin", &id, 0).unwrap();
        assert!(store.list("a", 0).unwrap().is_empty());
        assert!(store.list("b", 0).unwrap().is_empty());
    }
    #[test]
    fn quota_reserves_declared_data_and_metadata_before_accepting_uploads() {
        let (_fixture, mut store) = Fixture::new();
        let manifest = manifest(b"a");
        let id = store.begin("a", &manifest, 0).unwrap();
        let mut index = store.load().unwrap();
        index.uploads[0].reserved = QUOTA;
        store.save(&index).unwrap();
        assert!(store
            .begin("b", &manifest, 0)
            .unwrap_err()
            .contains("quota"));
        assert!(store.storage(&id).unwrap().exists());
    }
    #[test]
    fn renamed_worker_credentials_cannot_be_committed() {
        let (_fixture, mut store) = Fixture::new();
        let bytes = br#"{"server":"http://localhost:7331","token":"test-credential","workerId":"w","tasks":{}}"#;
        let upload = store.begin("a", &manifest(bytes), 0).unwrap();
        store.chunk("a", &upload, 0, 0, bytes, 0).unwrap();
        assert!(store
            .commit("a", &upload, 0)
            .unwrap_err()
            .contains("credential"));
    }
    #[test]
    fn interrupted_partial_chunk_is_resumed_only_for_the_same_bytes() {
        let (_fixture, mut store) = Fixture::new();
        let upload = store.begin("a", &manifest(b"abcdef"), 0).unwrap();
        fs::write(store.storage(&upload).unwrap().join("0.bin"), b"abc").unwrap();
        assert!(store.chunk("a", &upload, 0, 0, b"baddef", 1).is_err());
        store.chunk("a", &upload, 0, 0, b"abcdef", 1).unwrap();
        assert!(store.commit("a", &upload, 1).is_ok());
    }
}
