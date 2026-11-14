//! Portable source-snapshot protocol primitives; no filesystem or execution access.
//!
//! Version 1 intentionally allows ASCII paths only. This avoids Unicode filesystem
//! normalization disagreements, and rejects names that collide on Windows/macOS.
//! The mandatory path exclusions are conservative; they are not content scanning
//! and cannot establish that an otherwise allowed source file contains no secrets.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

type Result<T> = std::result::Result<T, String>;

pub const MAX_FILES: usize = 4096;
pub const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_SNAPSHOT_BYTES: u64 = 128 * 1024 * 1024;
pub const MAX_MANIFEST_BYTES: usize = 512 * 1024;
pub const CHUNK_BYTES: usize = 256 * 1024;
pub const MAX_PATH_BYTES: usize = 240;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SnapshotManifest {
    pub version: u32,
    pub files: Vec<SnapshotFile>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SnapshotFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
    #[serde(default)]
    pub executable: bool,
}

/// Non-overridable exclusions shared by collection and received manifests.
/// Any matching component excludes that file and its descendants.
pub fn excluded_path(path: &str) -> bool {
    path.split('/').any(|component| {
        let name = component.to_ascii_lowercase();
        matches!(
            name.as_str(),
            ".git"
                | ".hg"
                | ".svn"
                | "node_modules"
                | "target"
                | ".cargo-home"
                | ".sharebox"
                | ".sharebox-ui"
                | ".sharebox-snapshots"
                | ".sharebox-uploads"
                | ".sharebox-staging"
                | ".cache"
                | ".next"
                | ".nuxt"
                | ".turbo"
                | ".parcel-cache"
                | ".pytest_cache"
                | "__pycache__"
                | ".venv"
                | "venv"
                | ".npm"
                | ".pnpm-store"
                | ".yarn"
                | ".npmrc"
                | ".yarnrc"
                | ".yarnrc.yml"
                | ".netrc"
                | "_netrc"
                | ".ssh"
                | ".aws"
                | ".azure"
                | ".gcloud"
                | "gcloud"
                | ".kube"
                | ".docker"
                | ".gnupg"
                | ".git-credentials"
                | ".gitconfig"
                | "authorized_keys"
                | "known_hosts"
                | "identity"
                | "credentials"
                | "credentials.json"
                | "auth.json"
                | "application_default_credentials.json"
                | "service-account.json"
                | "service_account.json"
                | "worker.json"
                | "sharebox-worker.json"
                | "worker.credentials.json"
                | "admin-token"
                | "kubeconfig"
                | ".boto"
                | ".s3cfg"
                | ".pypirc"
                | "secrets"
                | ".secrets"
                | "secrets.json"
                | "secrets.yaml"
                | "secrets.yml"
                | "secrets.toml"
        ) || name.starts_with(".env")
            || name.starts_with("id_rsa")
            || name.starts_with("id_dsa")
            || name.starts_with("id_ecdsa")
            || name.starts_with("id_ed25519")
            || name.ends_with(".credentials.json")
            || [
                ".pem",
                ".key",
                ".p8",
                ".ppk",
                ".p12",
                ".pfx",
                ".jks",
                ".keystore",
            ]
            .iter()
            .any(|suffix| name.ends_with(suffix))
    })
}

/// Recognize a paired Sharebox worker configuration even when renamed. This is
/// a narrow credential signature, not a general secret-content detector.
pub fn contains_worker_credentials(bytes: &[u8]) -> bool {
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return true;
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return false;
    };
    value["token"]
        .as_str()
        .is_some_and(|token| !token.is_empty())
        && value["server"]
            .as_str()
            .is_some_and(|server| !server.is_empty())
        && (value["workerId"].as_str().is_some_and(|id| !id.is_empty())
            || value["tasks"].is_object())
}

/// Validate an untrusted slash-separated relative path without touching the disk.
/// Callers must separately reject symlinks/reparse points during filesystem access.
pub fn validate_path(path: &str) -> Result<()> {
    if path.is_empty() || path.len() > MAX_PATH_BYTES {
        return Err(format!(
            "Snapshot paths must contain 1 to {MAX_PATH_BYTES} bytes"
        ));
    }
    if !path.is_ascii() {
        return Err("Snapshot version 1 supports ASCII paths only".into());
    }
    if path.bytes().any(|byte| byte.is_ascii_control())
        || path.contains(['\\', ':', '<', '>', '"', '|', '?', '*'])
    {
        return Err(
            "Snapshot path contains a control character or a non-portable filename character"
                .into(),
        );
    }
    for component in path.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(
                "Snapshot paths must be relative, without empty, dot, or parent components".into(),
            );
        }
        if component.ends_with(['.', ' ']) {
            return Err("Snapshot path components cannot end with a dot or space".into());
        }
        // Windows reserves these device names even when they have an extension.
        let stem = component
            .split('.')
            .next()
            .unwrap_or("")
            .trim_end_matches(' ')
            .to_ascii_uppercase();
        let numbered_device = ["COM", "LPT"].iter().any(|prefix| {
            stem.strip_prefix(prefix).is_some_and(|number| {
                number.len() == 1 && matches!(number.as_bytes()[0], b'1'..=b'9')
            })
        });
        if matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
        ) || numbered_device
        {
            return Err("Snapshot path contains a reserved Windows device name".into());
        }
    }
    if excluded_path(path) {
        return Err("Snapshot path is excluded by the mandatory credential/cache policy".into());
    }
    Ok(())
}

/// Return canonical UTF-8 JSON, with files ordered by their exact ASCII paths.
/// Missing executable flags normalize to false; unknown fields and a missing
/// version are rejected by deserialization. SHA-256 values must be lowercase.
pub fn validate_manifest(manifest: &SnapshotManifest) -> Result<Vec<u8>> {
    if manifest.version != 1 {
        return Err("Unsupported snapshot manifest version; expected version 1".into());
    }
    if manifest.files.len() > MAX_FILES {
        return Err(format!("Snapshot exceeds the {MAX_FILES} file limit"));
    }
    let mut total = 0u64;
    let mut names = BTreeSet::new();
    let mut directories = BTreeMap::new();
    for (index, file) in manifest.files.iter().enumerate() {
        validate_path(&file.path).map_err(|error| format!("Snapshot file {index}: {error}"))?;
        if file.size > MAX_FILE_BYTES {
            return Err(format!(
                "Snapshot file {index} exceeds the 16 MiB file limit"
            ));
        }
        total = total
            .checked_add(file.size)
            .ok_or("Snapshot size overflow")?;
        if total > MAX_SNAPSHOT_BYTES {
            return Err("Snapshot exceeds the 128 MiB total size limit".into());
        }
        if file.sha256.len() != 64
            || !file
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(format!(
                "Snapshot file {index} requires a lowercase 64-character SHA-256 digest"
            ));
        }
        for (offset, _) in file.path.match_indices('/') {
            let prefix = &file.path[..offset];
            if let Some(previous) = directories.insert(prefix.to_ascii_lowercase(), prefix) {
                if previous != prefix {
                    return Err("Snapshot contains case-colliding directory paths".into());
                }
            }
        }
        if !names.insert(file.path.to_ascii_lowercase()) {
            return Err("Snapshot contains duplicate or case-colliding file paths".into());
        }
    }
    // Checking every ancestor catches both input orders and non-adjacent sorted
    // names (e.g. a, a-file, a/child), unlike an adjacent-prefix comparison.
    for path in &names {
        for (offset, _) in path.match_indices('/') {
            if names.contains(&path[..offset]) {
                return Err("Snapshot contains a file/directory path collision".into());
            }
        }
    }
    let mut sorted = manifest.clone();
    sorted
        .files
        .sort_unstable_by(|left, right| left.path.cmp(&right.path));
    let bytes = serde_json::to_vec(&sorted).map_err(|_| "Could not encode snapshot manifest")?;
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err("Snapshot manifest exceeds the 512 KiB encoded size limit".into());
    }
    Ok(bytes)
}

/// Lowercase SHA-256, used for both file bytes and canonical manifest bytes.
pub fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        hex.push(DIGITS[(byte >> 4) as usize] as char);
        hex.push(DIGITS[(byte & 15) as usize] as char);
    }
    hex
}

pub fn snapshot_id(manifest: &SnapshotManifest) -> Result<String> {
    validate_manifest(manifest).map(|bytes| hash_bytes(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn file(path: &str) -> SnapshotFile {
        SnapshotFile {
            path: path.into(),
            size: 3,
            sha256: hash_bytes(b"abc"),
            executable: false,
        }
    }
    fn manifest(paths: &[&str]) -> SnapshotManifest {
        SnapshotManifest {
            version: 1,
            files: paths.iter().map(|path| file(path)).collect(),
        }
    }
    #[test]
    fn sha256_known_vectors_and_canonical_manifest_order() {
        assert_eq!(
            hash_bytes(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hash_bytes(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let first = manifest(&["b.js", "a.js"]);
        let second = manifest(&["a.js", "b.js"]);
        assert_eq!(
            validate_manifest(&first).unwrap(),
            validate_manifest(&second).unwrap()
        );
        assert_eq!(
            snapshot_id(&first).unwrap(),
            hash_bytes(&validate_manifest(&second).unwrap())
        );
        let mut executable = second.clone();
        executable.files[0].executable = true;
        assert_ne!(
            snapshot_id(&second).unwrap(),
            snapshot_id(&executable).unwrap()
        );
    }
    #[test]
    fn rejects_traversal_windows_aliases_and_non_ascii() {
        for path in [
            "",
            "/etc/passwd",
            "../a",
            "a/../b",
            "./a",
            "a//b",
            "a/",
            "C:/a",
            "C:a",
            "a\\b",
            "\\\\host\\file",
            "a\0b",
            "a\nb",
            "a?b",
            "a*b",
            "a<b",
            "a>b",
            "a|b",
            "a\"b",
            "a.",
            "a /b",
            "con",
            "NuL.txt",
            "AUX.js",
            "COM1/a",
            "lpt9.log",
            "con .txt",
            "conin$",
            "caf\u{e9}.js",
        ] {
            assert!(validate_path(path).is_err(), "accepted {path:?}");
        }
        for path in [
            "src/index.ts",
            ".github/workflows/test.yml",
            "package-lock.json",
            "test fixture/a.js",
            "COM10.txt",
            "context.js",
        ] {
            assert!(validate_path(path).is_ok(), "rejected {path:?}");
        }
    }
    #[test]
    fn mandatory_exclusions_apply_to_any_case_and_depth() {
        for path in [
            ".git/config",
            "sharebox-worker.json",
            ".sharebox-snapshots/job/source/file.js",
            ".sharebox-uploads/a",
            ".sharebox-staging/a",
            "nested/NODE_MODULES/pkg/index.js",
            "a/.SHAREBOX/state.json",
            ".env",
            ".env.example",
            "nested/.ENV.production",
            "nested/.npmrc",
            ".netrc",
            ".ssh/id_ed25519",
            "a/.aws/credentials",
            "x/.config/gcloud/token",
            "a/server.KEY",
            "a/cert.pem",
            "worker.json",
            "nested/WORKER.credentials.json",
            "custom.credentials.json",
            "target/debug/app",
            "a/__pycache__/file.pyc",
            "id_rsa.pub",
        ] {
            assert!(excluded_path(path), "not excluded {path}");
            assert!(validate_path(path).is_err());
        }
    }
    #[test]
    fn rejects_case_collisions_and_file_directory_collisions_in_any_order() {
        for paths in [
            vec!["a.js", "A.js"],
            vec!["a.js", "a.js"],
            vec!["a/b", "A"],
            vec!["a", "a-file", "a/b"],
            vec!["a/B/c", "A/b"],
        ] {
            assert!(validate_manifest(&manifest(&paths)).is_err());
        }
    }
    #[test]
    fn validates_versions_digests_sizes_and_count_limits() {
        let mut value = manifest(&["a"]);
        value.version = 2;
        assert!(validate_manifest(&value).is_err());
        value.version = 1;
        for digest in ["a".repeat(63), "A".repeat(64), "g".repeat(64)] {
            value.files[0].sha256 = digest;
            assert!(validate_manifest(&value).is_err());
        }
        value.files[0] = file("a");
        value.files[0].size = MAX_FILE_BYTES;
        assert!(validate_manifest(&value).is_ok());
        value.files[0].size += 1;
        assert!(validate_manifest(&value).is_err());
        value.files = (0..9)
            .map(|index| {
                let mut file = file(&format!("f{index}"));
                file.size = MAX_FILE_BYTES;
                file
            })
            .collect();
        assert!(validate_manifest(&value).is_err());
        value.files.pop();
        assert!(validate_manifest(&value).is_ok());
        value.files = (0..=MAX_FILES)
            .map(|index| file(&format!("f{index}")))
            .collect();
        assert!(validate_manifest(&value).is_err());
    }
    #[test]
    fn rejects_oversized_encoded_manifest_and_paths() {
        let long = "a".repeat(MAX_PATH_BYTES + 1);
        assert!(validate_path(&long).is_err());
        let manifest = SnapshotManifest {
            version: 1,
            files: (0..2000)
                .map(|index| file(&format!("{}-{index}", "a".repeat(230))))
                .collect(),
        };
        assert!(validate_manifest(&manifest)
            .unwrap_err()
            .contains("512 KiB"));
        assert_eq!(CHUNK_BYTES, 256 * 1024);
    }
    #[test]
    fn detects_paired_worker_credentials_without_reporting_them() {
        assert!(contains_worker_credentials(br#"{"server":"http://127.0.0.1:7331","token":"test-token","workerId":"worker-a","tasks":{}}"#));
        assert!(!contains_worker_credentials(
            br#"{"server":"http://127.0.0.1:7331","token":"","tasks":{}}"#
        ));
        assert!(!contains_worker_credentials(b"ordinary source file"));
    }
    #[test]
    fn rejects_unknown_fields_and_requires_version_but_defaults_executable() {
        assert!(serde_json::from_str::<SnapshotManifest>(r#"{"files":[]}"#).is_err());
        assert!(
            serde_json::from_str::<SnapshotManifest>(r#"{"version":1,"files":[],"root":"/"}"#)
                .is_err()
        );
        assert!(serde_json::from_str::<SnapshotFile>(
            r#"{"path":"a","size":0,"sha256":"x","symlink":"/"}"#
        )
        .is_err());
        let file: SnapshotFile = serde_json::from_str(&format!(
            r#"{{"path":"a","size":0,"sha256":"{}"}}"#,
            hash_bytes(b"")
        ))
        .unwrap();
        assert!(!file.executable);
        assert!(validate_manifest(&SnapshotManifest {
            version: 1,
            files: vec![file]
        })
        .is_ok());
    }
}
