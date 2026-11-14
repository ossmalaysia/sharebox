//! Collect a local source tree into private, immutable-by-convention upload files.
//! This module never executes source or writes into the source directory.
use crate::snapshot::{
    contains_worker_credentials, excluded_path, hash_bytes, validate_manifest, validate_path,
    SnapshotFile, SnapshotManifest, MAX_FILES, MAX_FILE_BYTES, MAX_SNAPSHOT_BYTES,
};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

type Result<T> = std::result::Result<T, String>;
const MAX_ENTRIES: usize = 32_768;

struct Candidate {
    relative: PathBuf,
    portable: String,
    metadata: Metadata,
}

fn portable(path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(
                part.to_str()
                    .ok_or("Snapshot paths must contain valid UTF-8")?,
            ),
            _ => return Err("Snapshot path is not a relative file path".into()),
        }
    }
    let result = parts.join("/");
    Ok(result)
}

fn reparse(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        false
    }
}

fn regular(metadata: &Metadata) -> Result<()> {
    if metadata.file_type().is_symlink() || reparse(metadata) || !metadata.is_file() {
        return Err(
            "Snapshots accept regular files only; links and special files are rejected".into(),
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err("Hard-linked source files are not accepted".into());
        }
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err("Source file exceeds the snapshot file-size limit".into());
    }
    Ok(())
}

fn executable(metadata: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        false
    }
}

fn unchanged(before: &Metadata, after: &Metadata) -> Result<()> {
    let same = before.len() == after.len()
        && before.modified().map_err(|e| e.to_string())?
            == after.modified().map_err(|e| e.to_string())?
        && executable(before) == executable(after);
    #[cfg(unix)]
    let same = {
        use std::os::unix::fs::MetadataExt;
        same && before.dev() == after.dev() && before.ino() == after.ino()
    };
    if same {
        Ok(())
    } else {
        Err("Source changed during snapshot collection; retry after edits finish".into())
    }
}

fn open_root(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Lock directory identity against renames/reparse replacement while held.
        options.share_mode(1).custom_flags(0x02000000 | 0x00200000);
    }
    let file = options
        .open(path)
        .map_err(|e| format!("Open source root: {e}"))?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_dir() || reparse(&metadata) {
        return Err("Source root must be a regular directory, not a reparse point".into());
    }
    Ok(file)
}

// Keep ancestor handles alive until the file has been fully read. On Unix every
// open is relative to an already-open directory with O_NOFOLLOW. On Windows,
// deny deletion/write sharing on ancestors and reject all reparse points.
struct SourceFile {
    file: File,
    _ancestors: Vec<File>,
}

#[cfg(unix)]
fn open_source(root: &File, _root_path: &Path, relative: &Path) -> Result<SourceFile> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    let components: Vec<_> = relative.components().collect();
    let mut ancestors: Vec<File> = Vec::new();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err("Invalid source path component".into());
        };
        let name = CString::new(name.as_bytes()).map_err(|_| "Invalid source path")?;
        let parent = ancestors.last().unwrap_or(root);
        let last = index + 1 == components.len();
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if last { 0 } else { libc::O_DIRECTORY };
        let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(format!(
                "Open source without following links: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: openat returned a new, uniquely owned descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        if last {
            return Ok(SourceFile {
                file,
                _ancestors: ancestors,
            });
        }
        ancestors.push(file);
    }
    Err("Empty source path".into())
}

#[cfg(windows)]
fn open_source(_root: &File, root_path: &Path, relative: &Path) -> Result<SourceFile> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    let components: Vec<_> = relative.components().collect();
    let mut path = root_path.to_path_buf();
    let mut ancestors = Vec::new();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err("Invalid source path component".into());
        };
        path.push(name);
        let last = index + 1 == components.len();
        let file = OpenOptions::new()
            .read(true)
            .share_mode(1)
            .custom_flags(0x00200000 | if last { 0 } else { 0x02000000 })
            .open(&path)
            .map_err(|e| format!("Open source without following reparse points: {e}"))?;
        let metadata = file.metadata().map_err(|e| e.to_string())?;
        if reparse(&metadata) || (!last && !metadata.is_dir()) {
            return Err("Source contains a reparse point or non-directory ancestor".into());
        }
        if last {
            let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
            if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
                return Err(format!(
                    "Inspect source file links: {}",
                    std::io::Error::last_os_error()
                ));
            }
            if info.nNumberOfLinks != 1 {
                return Err("Hard-linked source files are not accepted".into());
            }
            return Ok(SourceFile {
                file,
                _ancestors: ancestors,
            });
        }
        ancestors.push(file);
    }
    Err("Empty source path".into())
}

#[cfg(not(any(unix, windows)))]
fn open_source(_: &File, _: &Path, _: &Path) -> Result<SourceFile> {
    Err("Safe snapshot collection is supported on Unix and Windows only".into())
}

const MAX_IGNORE_BYTES: u64 = 64 * 1024;
const MAX_TOTAL_IGNORE_BYTES: u64 = 1024 * 1024;

#[derive(Default)]
struct Collection {
    candidates: Vec<Candidate>,
    entries: usize,
    bytes: u64,
    ignore_bytes: u64,
    git_rules: Vec<ignore::gitignore::Gitignore>,
    custom_rules: Vec<ignore::gitignore::Gitignore>,
}

fn read_ignore(
    root: &Path,
    root_file: &File,
    relative: &Path,
    rules_root: &Path,
    total: &mut u64,
) -> Result<Option<ignore::gitignore::Gitignore>> {
    let path = root.join(relative);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None)
        }
        Err(error) => return Err(format!("Inspect ignore file: {error}")),
    };
    regular(&metadata)?;
    if metadata.len() > MAX_IGNORE_BYTES {
        return Err("Ignore file exceeds 64 KiB".into());
    }
    let source = open_source(root_file, root, relative)?;
    let before = source.file.metadata().map_err(|e| e.to_string())?;
    regular(&before)?;
    unchanged(&metadata, &before)?;
    let mut bytes = Vec::new();
    (&source.file)
        .take(MAX_IGNORE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("Read ignore file: {e}"))?;
    let after = source.file.metadata().map_err(|e| e.to_string())?;
    unchanged(&before, &after)?;
    if bytes.len() as u64 > MAX_IGNORE_BYTES || bytes.len() as u64 != before.len() {
        return Err("Ignore file changed or exceeds 64 KiB".into());
    }
    *total = total
        .checked_add(bytes.len() as u64)
        .ok_or("Ignore size overflow")?;
    if *total > MAX_TOTAL_IGNORE_BYTES {
        return Err("Combined ignore files exceed 1 MiB".into());
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| "Ignore files must contain valid UTF-8")?;
    let mut builder = ignore::gitignore::GitignoreBuilder::new(rules_root);
    for (index, line) in text.lines().enumerate() {
        if index >= 4096 {
            return Err("Ignore file has too many lines".into());
        }
        builder
            .add_line(None, line)
            .map_err(|_| format!("Invalid ignore pattern at line {}", index + 1))?;
    }
    Ok(Some(
        builder
            .build()
            .map_err(|_| "Cannot compile ignore patterns")?,
    ))
}

fn ignored(rules: &[ignore::gitignore::Gitignore], path: &Path, is_dir: bool) -> Option<bool> {
    for rule in rules.iter().rev() {
        match rule.matched(path, is_dir) {
            ignore::Match::Ignore(_) => return Some(true),
            ignore::Match::Whitelist(_) => return Some(false),
            ignore::Match::None => {}
        }
    }
    None
}

fn visit(
    root: &Path,
    root_file: &File,
    relative: &Path,
    collection: &mut Collection,
) -> Result<()> {
    let directory = root.join(relative);
    let old_git = collection.git_rules.len();
    let old_custom = collection.custom_rules.len();
    if let Some(rule) = read_ignore(
        root,
        root_file,
        &relative.join(".gitignore"),
        &directory,
        &mut collection.ignore_bytes,
    )? {
        collection.git_rules.push(rule);
    }
    if let Some(rule) = read_ignore(
        root,
        root_file,
        &relative.join(".ignore"),
        &directory,
        &mut collection.ignore_bytes,
    )? {
        collection.custom_rules.push(rule);
    }
    // Stream directory entries so an enormous directory cannot first allocate an
    // unbounded entry vector. Only the bounded final file list needs sorting.
    for entry in fs::read_dir(&directory).map_err(|e| format!("Read source directory: {e}"))? {
        let entry = entry.map_err(|e| format!("Read source entry: {e}"))?;
        collection.entries += 1;
        if collection.entries > MAX_ENTRIES {
            return Err("Source tree contains too many entries".into());
        }
        let relative = relative.join(entry.file_name());
        let name = portable(&relative)?;
        if excluded_path(&name) {
            continue;
        }
        validate_path(&name)?;
        let metadata =
            fs::symlink_metadata(entry.path()).map_err(|e| format!("Inspect source entry: {e}"))?;
        if ignored(&collection.custom_rules, &entry.path(), metadata.is_dir())
            .or_else(|| ignored(&collection.git_rules, &entry.path(), metadata.is_dir()))
            .unwrap_or(false)
        {
            continue;
        }
        if metadata.file_type().is_symlink() || reparse(&metadata) {
            return Err(format!("Source links are not accepted: {name}"));
        }
        if metadata.is_dir() {
            visit(root, root_file, &relative, collection)?;
            continue;
        }
        regular(&metadata)?;
        if collection.candidates.len() >= MAX_FILES {
            return Err("Source exceeds the snapshot file-count limit".into());
        }
        collection.bytes = collection
            .bytes
            .checked_add(metadata.len())
            .ok_or("Source size overflow")?;
        if collection.bytes > MAX_SNAPSHOT_BYTES {
            return Err("Source exceeds the snapshot total-size limit".into());
        }
        collection.candidates.push(Candidate {
            relative,
            portable: name,
            metadata,
        });
    }
    collection.git_rules.truncate(old_git);
    collection.custom_rules.truncate(old_custom);
    Ok(())
}

fn collect(root: &Path, root_file: &File) -> Result<Vec<Candidate>> {
    let mut collection = Collection::default();
    // Git's local exclude file has lower priority than per-directory rules.
    if let Some(rule) = read_ignore(
        root,
        root_file,
        Path::new(".git/info/exclude"),
        root,
        &mut collection.ignore_bytes,
    )? {
        collection.git_rules.push(rule);
    }
    visit(root, root_file, Path::new(""), &mut collection)?;
    collection
        .candidates
        .sort_by(|a, b| a.portable.cmp(&b.portable));
    Ok(collection.candidates)
}

/// Stage bytes once under `<output>/<manifest index>.bin`. Output must be a new
/// directory outside the source tree, under an existing caller-owned parent.
/// Any error after creation removes this stage; the input tree is never modified.
pub fn stage(directory: &Path, output: &Path) -> Result<SnapshotManifest> {
    let metadata =
        fs::symlink_metadata(directory).map_err(|e| format!("Inspect source directory: {e}"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() || reparse(&metadata) {
        return Err("Source must be a directory, not a link or reparse point".into());
    }
    let root = directory
        .canonicalize()
        .map_err(|e| format!("Resolve source directory: {e}"))?;
    let root_file = open_root(&root)?;
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = output
        .file_name()
        .ok_or("Stage output needs a directory name")?;
    let output = parent
        .canonicalize()
        .map_err(|e| format!("Resolve stage parent: {e}"))?
        .join(name);
    if output.starts_with(&root) {
        return Err("Stage output must be outside the source tree".into());
    }
    #[cfg(unix)]
    let created = {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(&output)
    };
    #[cfg(not(unix))]
    let created = fs::create_dir(&output);
    created.map_err(|e| format!("Create fresh stage directory: {e}"))?;
    let result = (|| {
        let candidates = collect(&root, &root_file)?;
        let mut manifest = SnapshotManifest {
            version: 1,
            files: Vec::new(),
        };
        let mut total = 0u64;
        for (index, candidate) in candidates.iter().enumerate() {
            let source = open_source(&root_file, &root, &candidate.relative)?;
            let before = source.file.metadata().map_err(|e| e.to_string())?;
            regular(&before)?;
            unchanged(&candidate.metadata, &before)?;
            let mut bytes = Vec::new();
            (&source.file)
                .take(MAX_FILE_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| format!("Read source file: {e}"))?;
            let after = source.file.metadata().map_err(|e| e.to_string())?;
            regular(&after)?;
            unchanged(&before, &after)?;
            if bytes.len() as u64 != before.len() || bytes.len() as u64 > MAX_FILE_BYTES {
                return Err("Source size changed or exceeded the file-size limit".into());
            }
            total = total
                .checked_add(bytes.len() as u64)
                .ok_or("Source size overflow")?;
            if total > MAX_SNAPSHOT_BYTES {
                return Err("Source exceeds the total-size limit".into());
            }
            if contains_worker_credentials(&bytes) {
                return Err(format!(
                    "Refusing Sharebox worker credentials in source file: {}",
                    candidate.portable
                ));
            }
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut staged = options
                .open(output.join(format!("{index}.bin")))
                .map_err(|e| format!("Create stage file: {e}"))?;
            staged
                .write_all(&bytes)
                .and_then(|_| staged.sync_all())
                .map_err(|e| format!("Persist stage file: {e}"))?;
            manifest.files.push(SnapshotFile {
                path: candidate.portable.clone(),
                size: bytes.len() as u64,
                sha256: hash_bytes(&bytes),
                executable: executable(&before),
            });
        }
        validate_manifest(&manifest)?;
        #[cfg(unix)]
        File::open(&output)
            .and_then(|file| file.sync_all())
            .map_err(|e| format!("Persist stage directory: {e}"))?;
        Ok(manifest)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&output);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let mut bytes = [0u8; 16];
            getrandom::getrandom(&mut bytes).unwrap();
            let nonce: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            let root = std::env::temp_dir().join(format!("sharebox-collect-{nonce}"));
            fs::create_dir(&root).unwrap();
            fs::create_dir(root.join("source")).unwrap();
            Self(root)
        }
        fn source(&self) -> PathBuf {
            self.0.join("source")
        }
        fn output(&self) -> PathBuf {
            self.0.join("stage")
        }
        fn write(&self, path: &str, data: &[u8]) {
            let target = self.source().join(path);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(target, data).unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn respects_gitignore_and_secret_exclusions_and_stages_exact_bytes() {
        let fixture = Fixture::new();
        fixture.write(".gitignore", b"ignored/\n!node_modules/\n!.env\n");
        fixture.write("ignored/data.js", b"ignored");
        fixture.write("node_modules/dependency/index.js", b"excluded");
        fixture.write(".env", b"FAKE_EXCLUDED_VALUE=fixture-only");
        fixture.write(".git/config", b"[core]\nrepositoryformatversion = 0\n");
        fixture.write("src/test.js", b"console.log('staged')\n");
        let manifest = stage(&fixture.source(), &fixture.output()).unwrap();
        assert_eq!(
            manifest
                .files
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>(),
            [".gitignore", "src/test.js"]
        );
        for (index, item) in manifest.files.iter().enumerate() {
            let bytes = fs::read(fixture.output().join(format!("{index}.bin"))).unwrap();
            assert_eq!(hash_bytes(&bytes), item.sha256);
            assert_eq!(bytes.len() as u64, item.size);
        }
        fixture.write("src/test.js", b"later edit");
        assert_eq!(
            fs::read(fixture.output().join("1.bin")).unwrap(),
            b"console.log('staged')\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(fixture.output()).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(fixture.output().join("1.bin"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn bounds_ignore_files_and_honors_nested_negation() {
        let fixture = Fixture::new();
        fixture.write(".gitignore", b"*.log\n");
        fixture.write("nested/.gitignore", b"!keep.log\n");
        fixture.write("nested/keep.log", b"include");
        fixture.write("nested/drop.log", b"exclude");
        let manifest = stage(&fixture.source(), &fixture.output()).unwrap();
        assert!(manifest
            .files
            .iter()
            .any(|file| file.path == "nested/keep.log"));
        assert!(!manifest
            .files
            .iter()
            .any(|file| file.path == "nested/drop.log"));
        fs::remove_dir_all(fixture.output()).unwrap();
        fixture.write(
            "nested/.gitignore",
            &vec![b'x'; MAX_IGNORE_BYTES as usize + 1],
        );
        assert!(stage(&fixture.source(), &fixture.output())
            .unwrap_err()
            .contains("64 KiB"));
        assert!(!fixture.output().exists());
    }

    #[test]
    fn refuses_renamed_worker_credentials_without_printing_the_token() {
        let fixture = Fixture::new();
        let credential = serde_json::json!({"token":"fixture-private-token", "workerId":"worker-fixture", "server":"http://127.0.0.1:7331", "tasks":{}});
        fixture.write("settings.json", &serde_json::to_vec(&credential).unwrap());
        let error = stage(&fixture.source(), &fixture.output()).unwrap_err();
        assert!(error.contains("settings.json"));
        assert!(!error.contains("fixture-private-token"));
        assert!(!fixture.output().exists());
        fixture.write(
            "settings.json",
            br#"{"token":"application-fixture","name":"sample"}"#,
        );
        assert!(stage(&fixture.source(), &fixture.output()).is_ok());
    }

    #[test]
    fn refuses_existing_or_nested_output_without_deleting_it() {
        let fixture = Fixture::new();
        fixture.write("test.js", b"test");
        assert!(stage(&fixture.source(), &fixture.source().join("nested")).is_err());
        assert!(!fixture.source().join("nested").exists());
        fs::create_dir(fixture.output()).unwrap();
        fs::write(fixture.output().join("keep"), b"keep").unwrap();
        assert!(stage(&fixture.source(), &fixture.output()).is_err());
        assert_eq!(fs::read(fixture.output().join("keep")).unwrap(), b"keep");
    }

    #[test]
    fn rejects_hardlinks_and_cleans_partial_stage() {
        let fixture = Fixture::new();
        fixture.write("first.js", b"test");
        fs::hard_link(
            fixture.source().join("first.js"),
            fixture.source().join("second.js"),
        )
        .unwrap();
        assert!(stage(&fixture.source(), &fixture.output()).is_err());
        assert!(!fixture.output().exists());
    }

    #[test]
    fn rejects_oversized_files_and_cleans_stage() {
        let fixture = Fixture::new();
        fixture.write("large", b"");
        OpenOptions::new()
            .write(true)
            .open(fixture.source().join("large"))
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        assert!(stage(&fixture.source(), &fixture.output()).is_err());
        assert!(!fixture.output().exists());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_files_and_swapped_ancestor() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        fixture.write("real/test.js", b"test");
        symlink("real/test.js", fixture.source().join("link.js")).unwrap();
        assert!(stage(&fixture.source(), &fixture.output()).is_err());
        assert!(!fixture.output().exists());
        let root = open_root(&fixture.source()).unwrap();
        symlink("real", fixture.source().join("alias")).unwrap();
        assert!(open_source(&root, &fixture.source(), Path::new("alias/test.js")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_special_files_and_preserves_only_executable_flag() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        fixture.write("run.sh", b"exit 0\n");
        fs::set_permissions(
            fixture.source().join("run.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let manifest = stage(&fixture.source(), &fixture.output()).unwrap();
        assert!(manifest.files[0].executable);
        fs::remove_dir_all(fixture.output()).unwrap();
        use std::os::unix::ffi::OsStrExt;
        let fifo = fixture.source().join("fifo");
        let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(stage(&fixture.source(), &fixture.output()).is_err());
        assert!(!fixture.output().exists());
    }
}
