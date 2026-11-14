//! Local source collection and bounded upload; no remote execution is started.
use serde_json::{json, Value};
use sharebox::snapshot::{
    hash_bytes, snapshot_id, validate_manifest, SnapshotManifest, CHUNK_BYTES, MAX_FILE_BYTES,
};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

type Result<T> = std::result::Result<T, String>;
pub const HELP: &str = "sharebox snapshot inspect [--directory PATH]\nsharebox snapshot upload [--directory PATH] --server http://MASTER_IP:PORT --token-env ENV_NAME\n\nInspect lists the exact staged manifest without uploading. Upload prints a snapshot ID for MCP jobs.\nTokens are read from an existing environment variable, never a literal command argument.\nOnly localhost/private IPv4 masters are supported. Source exclusions cannot detect every secret in code.";

struct Options {
    action: String,
    directory: PathBuf,
    server: Option<String>,
    token_env: Option<String>,
}

fn valid_env_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_alphabetic() || (index > 0 && byte.is_ascii_digit())
        })
}
fn validate_server(server: &str) -> Result<()> {
    let authority = server
        .strip_prefix("http://")
        .ok_or("Use an http:// private LAN master URL")?
        .trim_end_matches('/');
    let accepted = if let Some(port) = authority.strip_prefix("localhost:") {
        port.parse::<u16>().is_ok_and(|port| port != 0)
    } else {
        authority
            .parse::<std::net::SocketAddrV4>()
            .is_ok_and(|address| {
                address.port() != 0 && (address.ip().is_private() || address.ip().is_loopback())
            })
    };
    if accepted {
        Ok(())
    } else {
        Err("Master URL must use localhost or a private/loopback IPv4 address with an explicit nonzero port".into())
    }
}
fn parse(args: Vec<String>) -> Result<Options> {
    let mut args = args.into_iter();
    let action = args.next().ok_or(HELP)?;
    if action != "upload" && action != "inspect" {
        return Err(HELP.into());
    }
    let mut options = HashMap::new();
    while let Some(key) = args.next() {
        if key != "--directory"
            && !(action == "upload" && matches!(key.as_str(), "--server" | "--token-env"))
        {
            return Err("Unsupported snapshot option; use sharebox snapshot --help".into());
        }
        let value = args
            .next()
            .ok_or_else(|| format!("Missing value for {key}"))?;
        if options.insert(key.clone(), value).is_some() {
            return Err(format!("Duplicate option {key}"));
        }
    }
    let directory = PathBuf::from(options.remove("--directory").unwrap_or_else(|| ".".into()));
    let server = options.remove("--server");
    let token_env = options.remove("--token-env");
    if action == "upload" {
        validate_server(server.as_deref().ok_or("Upload requires --server")?)?;
        if !valid_env_name(token_env.as_deref().ok_or("Upload requires --token-env")?) {
            return Err("Invalid token environment variable name".into());
        }
    }
    Ok(Options {
        action,
        directory,
        server,
        token_env,
    })
}

struct TemporaryStage(PathBuf);
impl TemporaryStage {
    fn new() -> Result<Self> {
        let mut bytes = [0u8; 16];
        getrandom::getrandom(&mut bytes).map_err(|_| "Secure randomness unavailable")?;
        let nonce: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        let path = std::env::temp_dir().join(format!("sharebox-snapshot-{nonce}"));
        #[cfg(unix)]
        let created = {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new().mode(0o700).create(&path)
        };
        #[cfg(not(unix))]
        let created = fs::create_dir(&path);
        created.map_err(|e| format!("Create private staging parent: {e}"))?;
        Ok(Self(path))
    }
    fn files(&self) -> PathBuf {
        self.0.join("files")
    }
}
impl Drop for TemporaryStage {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn retryable(error: &str) -> bool {
    match error
        .strip_prefix("HTTP ")
        .and_then(|rest| rest.split(':').next())
        .and_then(|status| status.parse::<u16>().ok())
    {
        Some(status) => status >= 500,
        None => true,
    }
}
fn request(
    server: &str,
    path: &str,
    token: &str,
    method: &str,
    content_type: &str,
    body: &[u8],
    retry: bool,
) -> Result<Value> {
    let attempts = if retry { 3 } else { 1 };
    for attempt in 0..attempts {
        match crate::http::client_raw(server, path, Some(token), method, content_type, body) {
            Ok(response) => {
                return serde_json::from_slice(&response)
                    .map_err(|_| "Master returned invalid JSON".into())
            }
            Err(error) if attempt + 1 < attempts && retryable(&error) => {
                std::thread::sleep(Duration::from_millis(200 * (attempt + 1)));
            }
            Err(error) => return Err(error.replace(token, "[REDACTED]")),
        }
    }
    Err("Upload request failed".into())
}

fn total_bytes(manifest: &SnapshotManifest) -> u64 {
    manifest.files.iter().map(|file| file.size).sum()
}
fn upload(manifest: &SnapshotManifest, stage: &Path, server: &str, token: &str) -> Result<Value> {
    let canonical = validate_manifest(manifest)?;
    let expected_id = snapshot_id(manifest)?;
    let response = request(
        server,
        "/api/snapshots/uploads",
        token,
        "POST",
        "application/json",
        &canonical,
        false,
    )?;
    let upload_id = response["uploadId"]
        .as_str()
        .filter(|id| {
            id.len() == 64
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        .ok_or("Master returned an invalid upload ID")?;
    for (index, entry) in manifest.files.iter().enumerate() {
        let mut bytes = Vec::new();
        File::open(stage.join(format!("{index}.bin")))
            .map_err(|e| format!("Open staged file: {e}"))?
            .take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("Read staged file: {e}"))?;
        if bytes.len() as u64 != entry.size || hash_bytes(&bytes) != entry.sha256 {
            return Err("Staged file changed after collection; upload stopped".into());
        }
        for (chunk_index, chunk) in bytes.chunks(CHUNK_BYTES).enumerate() {
            let path =
                format!("/api/snapshots/uploads/{upload_id}/files/{index}/chunks/{chunk_index}");
            request(
                server,
                &path,
                token,
                "PUT",
                "application/octet-stream",
                chunk,
                true,
            )?;
        }
    }
    let committed = request(
        server,
        &format!("/api/snapshots/uploads/{upload_id}/commit"),
        token,
        "POST",
        "application/json",
        b"{}",
        true,
    )?;
    if committed["snapshotId"].as_str() != Some(&expected_id)
        || committed["fileCount"].as_u64() != Some(manifest.files.len() as u64)
        || committed["totalBytes"].as_u64() != Some(total_bytes(manifest))
    {
        return Err("Master commit result does not match the staged snapshot".into());
    }
    Ok(
        json!({"snapshotId": expected_id, "fileCount": manifest.files.len(), "totalBytes": total_bytes(manifest)}),
    )
}

pub fn run(args: Vec<String>) -> Result<()> {
    if args.is_empty() || args == ["--help"] || args == ["-h"] || args == ["help"] {
        println!("{HELP}");
        return Ok(());
    }
    let options = parse(args)?;
    // Validate credentials before touching source or making an upload session.
    let token = if let Some(name) = &options.token_env {
        let token = std::env::var(name)
            .map_err(|_| "Token environment variable is unset or not Unicode")?;
        if token.is_empty()
            || token.len() > 4096
            || token.bytes().any(|byte| !byte.is_ascii_graphic())
        {
            return Err("Token environment variable contains an invalid bearer token".into());
        }
        Some(token)
    } else {
        None
    };
    let temporary = TemporaryStage::new()?;
    let manifest = sharebox::snapshot_collect::stage(&options.directory, &temporary.files())?;
    let summary = if options.action == "inspect" {
        json!({"snapshotId":snapshot_id(&manifest)?, "fileCount":manifest.files.len(), "totalBytes":total_bytes(&manifest), "files":manifest.files})
    } else {
        upload(
            &manifest,
            &temporary.files(),
            options.server.as_deref().ok_or("Missing master URL")?,
            token.as_deref().ok_or("Missing upload credential")?,
        )?
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&summary).map_err(|_| "Cannot encode snapshot summary")?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).into()).collect()
    }
    #[test]
    fn upload_requires_env_token_and_private_explicit_master() {
        assert!(parse(args(&[
            "upload",
            "--server",
            "http://192.168.1.3:7331",
            "--token-env",
            "SHAREBOX_TOKEN"
        ]))
        .is_ok());
        for command in [
            args(&[
                "upload",
                "--server",
                "http://8.8.8.8:7331",
                "--token-env",
                "TOKEN",
            ]),
            args(&[
                "upload",
                "--server",
                "http://127.0.0.1:7331",
                "--token",
                "secret",
            ]),
            args(&[
                "upload",
                "--server",
                "http://127.0.0.1:7331",
                "--token-env",
                "BAD=NAME",
            ]),
            args(&["upload", "--server", "http://127.0.0.1:7331"]),
            args(&["inspect", "--token-env", "TOKEN"]),
            args(&["inspect", "--directory", "a", "--directory", "b"]),
        ] {
            assert!(parse(command).is_err());
        }
    }
    #[test]
    fn auth_quota_and_conflict_errors_are_not_retried() {
        for status in [400, 401, 403, 404, 409, 413, 429] {
            assert!(!retryable(&format!("HTTP {status}: rejected")));
        }
        assert!(retryable("HTTP 503: unavailable"));
        assert!(retryable("connection reset"));
    }
    #[test]
    fn temporary_files_are_removed_on_error_return() {
        let mut directory = None;
        let outcome = (|| -> Result<()> {
            let temporary = TemporaryStage::new()?;
            directory = Some(temporary.0.clone());
            fs::write(temporary.0.join("partial"), b"staged bytes").map_err(|e| e.to_string())?;
            Err("synthetic failure".into())
        })();
        assert!(outcome.is_err());
        assert!(!directory.unwrap().exists());
    }
}
