use crate::http::{self, Request, Response};
use crate::snapshot_store::SnapshotStore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sharebox::snapshot::{SnapshotManifest, CHUNK_BYTES, MAX_MANIFEST_BYTES};
use std::{
    fs,
    io::Write,
    net::{IpAddr, SocketAddr, TcpListener},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
fn random() -> String {
    let mut b = [0u8; 32];
    getrandom::getrandom(&mut b).expect("OS secure randomness unavailable");
    b.iter().map(|x| format!("{x:02x}")).collect()
}
fn equal(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes().zip(b.bytes()).fold(0u8, |v, (a, b)| v | (a ^ b)) == 0
}
#[derive(Serialize, Deserialize, Default, Clone)]
struct State {
    workers: Vec<Value>,
    jobs: Vec<Value>,
    tokens: Vec<Value>,
    pairings: Vec<Value>,
}
struct Store {
    state: State,
    path: PathBuf,
    admin: String,
    bound_addr: SocketAddr,
    snapshots: Option<SnapshotStore>,
}
fn private_write(path: &Path, data: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    let mut opt = fs::OpenOptions::new();
    opt.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opt.mode(0o600);
    }
    let mut f = opt.open(&tmp).map_err(|e| e.to_string())?;
    f.write_all(data)
        .and_then(|_| f.sync_all())
        .map_err(|e| e.to_string())?;
    drop(f);
    crate::worker::replace_file(&tmp, path).map_err(|e| e.to_string())
}
impl Store {
    fn save(&self) -> Result<(), String> {
        private_write(
            &self.path,
            &serde_json::to_vec(&self.state).map_err(|e| e.to_string())?,
        )
    }
    fn role(&self, req: &Request) -> Role {
        let Some(t) = req
            .headers
            .get("authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
        else {
            return Role::None;
        };
        if equal(t, &self.admin) {
            return Role::Admin;
        }
        if let Some(token) = self
            .state
            .tokens
            .iter()
            .find(|x| equal(t, x["token"].as_str().unwrap_or("")))
        {
            if let Some(id) = token["id"].as_str() {
                return Role::Agent(id.into());
            }
        }
        if let Some(w) = self.state.workers.iter().find(|w| {
            !w["revoked"].as_bool().unwrap_or(false) && equal(t, w["token"].as_str().unwrap_or(""))
        }) {
            return Role::Worker(w["id"].as_str().unwrap().to_string());
        }
        Role::None
    }
    fn expire(&mut self) {
        let t = now();
        self.state
            .pairings
            .retain(|p| p["expiresAt"].as_u64().unwrap_or(0) > t);
        for job in &mut self.state.jobs {
            if ["running", "cancelling"].contains(&job["status"].as_str().unwrap_or(""))
                && t.saturating_sub(job["leaseAt"].as_u64().unwrap_or(0)) > 90_000
            {
                job["status"] = json!("lost");
                job["updatedAt"] = json!(t);
                job["log"] = json!(format!(
                    "{}\n[master] Worker heartbeat lost; job will not be automatically retried.\n",
                    job["log"].as_str().unwrap_or("")
                ));
            }
        }
    }
}
#[derive(PartialEq)]
enum Role {
    Admin,
    Agent(String),
    Worker(String),
    None,
}
fn clean(v: &Value) -> Value {
    let mut v = v.clone();
    if let Some(o) = v.as_object_mut() {
        o.remove("token");
        o.remove("requestKey");
        o.remove("sequence");
        o.remove("snapshotOwner");
        o.remove("requestOwner");
    }
    v
}
fn tasks(v: &Value) -> Result<Value, String> {
    let a = v.as_array().ok_or("tasks must be an array")?;
    if a.is_empty() || a.len() > 64 {
        return Err("Provide 1 to 64 tasks".into());
    }
    let mut names = std::collections::HashSet::new();
    for t in a {
        let n = t["name"].as_str().ok_or("Task name required")?;
        if n.is_empty() || n.len() > 80 || !names.insert(n) {
            return Err("Invalid or duplicate task name".into());
        }
        if t["description"].as_str().unwrap_or("").len() > 500 {
            return Err("Task description too long".into());
        }
    }
    for task in a {
        if task
            .get("snapshot")
            .is_some_and(|value| !value.is_boolean())
        {
            return Err("Task snapshot descriptor must be a boolean".into());
        }
        if task
            .get("execution")
            .is_some_and(|value| !value.is_string())
        {
            return Err("Task execution descriptor must be a string".into());
        }
        let execution = task
            .get("execution")
            .and_then(Value::as_str)
            .unwrap_or("native");
        if !["native", "docker"].contains(&execution)
            || (execution == "docker" && task["snapshot"] != true)
        {
            return Err("Task execution must be native, or docker with snapshot enabled".into());
        }
    }
    Ok(v.clone())
}
fn submit(store: &mut Store, principal: &str, b: &Value) -> Result<Value, String> {
    let worker_id = b["workerId"].as_str().ok_or("workerId required")?;
    let task = b["task"].as_str().ok_or("task required")?;
    let worker = store
        .state
        .workers
        .iter()
        .find(|w| w["id"] == worker_id && !w["revoked"].as_bool().unwrap_or(false))
        .ok_or("Worker not found")?;
    if now().saturating_sub(worker["lastSeen"].as_u64().unwrap_or(0)) > 30_000 {
        return Err("Worker is offline; start it before submitting".into());
    }
    let descriptor = worker["tasks"]
        .as_array()
        .and_then(|tasks| tasks.iter().find(|t| t["name"] == task))
        .ok_or("Task is not permitted on this worker")?;
    let snapshot_task = descriptor["snapshot"].as_bool().unwrap_or(false);
    let execution = descriptor["execution"]
        .as_str()
        .unwrap_or("native")
        .to_owned();
    if !["native", "docker"].contains(&execution.as_str())
        || (!snapshot_task && execution != "native")
    {
        return Err("Worker task has an unsupported execution descriptor".into());
    }
    let snapshot_id = match b.get("snapshotId") {
        Some(value) => Some(value.as_str().ok_or("snapshotId must be a string")?),
        None => None,
    };
    if snapshot_task != snapshot_id.is_some() {
        return Err(if snapshot_task {
            "This task requires a committed snapshotId"
        } else {
            "This task uses its local checkout and does not accept snapshotId"
        }
        .into());
    }
    let snapshot_owner = if let Some(id) = snapshot_id {
        let snapshots = store
            .snapshots
            .as_mut()
            .ok_or("Snapshot storage is unavailable")?;
        snapshots.owned_manifest(principal, id, now())?;
        Some(snapshots.resolve_owner(principal, id, now())?)
    } else {
        None
    };
    if let Some(key) = b["requestKey"].as_str() {
        if key.len() > 128 {
            return Err("requestKey too long".into());
        }
        if let Some(job) = store.state.jobs.iter().find(|j| j["requestKey"] == key) {
            if job["task"] != task
                || job["workerId"] != worker_id
                || job["snapshotId"].as_str() != snapshot_id
                || job["requestOwner"]
                    .as_str()
                    .is_some_and(|owner| owner != principal)
            {
                return Err(
                    "requestKey already used for a different task, snapshot, or principal".into(),
                );
            }
            return Ok(clean(job));
        }
    }
    if store.state.jobs.len() >= 500 {
        return Err(
            "Local history limit reached (500 jobs); archive state before adding more".into(),
        );
    }
    let mut job = json!({"id":format!("job-{}",&random()[..16]),"workerId":worker_id,"task":task,"status":"queued","createdAt":now(),"updatedAt":now(),"log":"","sequence":0,"requestKey":b["requestKey"],"requestOwner":principal,"execution":execution,"source":if snapshot_task {"snapshot"} else {"worker-local-checkout"}});
    if let Some(id) = snapshot_id {
        job["snapshotId"] = json!(id);
        job["snapshotOwner"] = json!(snapshot_owner);
    }
    store.state.jobs.push(job.clone());
    Ok(clean(&job))
}
fn delete_snapshot(store: &mut Store, principal: &str, id: &str) -> Result<Value, String> {
    if store.state.jobs.iter().any(|job| {
        (principal == "admin" || job["snapshotOwner"] == principal)
            && job["snapshotId"] == id
            && ["queued", "running", "cancelling"].contains(&job["status"].as_str().unwrap_or(""))
    }) {
        return Err(
            "Snapshot is referenced by an active job; finish or cancel that job before deleting it"
                .into(),
        );
    }
    store
        .snapshots
        .as_mut()
        .ok_or("Snapshot storage is unavailable")?
        .delete(principal, id, now())?;
    Ok(json!({"ok":true}))
}

fn cancel(s: &mut State, id: &str) -> Result<Value, String> {
    let j = s
        .jobs
        .iter_mut()
        .find(|j| j["id"] == id)
        .ok_or("Job not found")?;
    match j["status"].as_str().unwrap_or("") {
        "queued" => j["status"] = json!("cancelled"),
        "running" => j["status"] = json!("cancelling"),
        _ => {}
    }
    j["updatedAt"] = json!(now());
    Ok(clean(j))
}
fn snapshot_error(error: String) -> Response {
    let code = if error.contains("not found") || error.contains("expired") {
        404
    } else if error.contains("quota")
        || error.contains("limit reached")
        || error.contains("two active")
    {
        429
    } else if error.contains("active job") || error.contains("Conflicting") {
        409
    } else {
        400
    };
    Response::error(code, &error)
}
fn snapshot_api(store: &mut Store, principal: &str, req: &Request) -> Response {
    if req.method == "POST" && req.path == "/api/snapshots/uploads" {
        if req.body.len() > MAX_MANIFEST_BYTES {
            return Response::error(413, "Snapshot manifest exceeds 512 KiB");
        }
        let manifest: SnapshotManifest = match serde_json::from_slice(&req.body) {
            Ok(value) => value,
            Err(_) => return Response::error(400, "Invalid snapshot manifest JSON"),
        };
        return match store
            .snapshots
            .as_mut()
            .ok_or("Snapshot storage is unavailable".into())
            .and_then(|snapshots| snapshots.begin(principal, &manifest, now()))
        {
            Ok(id) => Response::json(201, json!({"uploadId":id})),
            Err(error) => snapshot_error(error),
        };
    }
    let parts: Vec<_> = req.path.trim_start_matches('/').split('/').collect();
    if req.method == "PUT" {
        if req.body.len() > CHUNK_BYTES {
            return Response::error(413, "Snapshot chunk exceeds 256 KiB");
        }
        if let ["api", "snapshots", "uploads", upload, "files", file, "chunks", chunk] =
            parts.as_slice()
        {
            let (Ok(file), Ok(chunk)) = (file.parse::<usize>(), chunk.parse::<usize>()) else {
                return Response::error(400, "Invalid snapshot file or chunk index");
            };
            return match store
                .snapshots
                .as_mut()
                .ok_or("Snapshot storage is unavailable".into())
                .and_then(|snapshots| {
                    snapshots.chunk(principal, upload, file, chunk, &req.body, now())
                }) {
                Ok(duplicate) => Response::json(200, json!({"ok":true,"duplicate":duplicate})),
                Err(error) => snapshot_error(error),
            };
        }
    }
    if req.method == "POST" {
        if !req.body.is_empty()
            && serde_json::from_slice::<Value>(&req.body).ok() != Some(json!({}))
        {
            return Response::error(400, "Expected an empty JSON object");
        }
        if req.path == "/api/snapshots/list" {
            return match store
                .snapshots
                .as_mut()
                .ok_or("Snapshot storage is unavailable".into())
                .and_then(|snapshots| snapshots.list(principal, now()))
            {
                Ok(snapshots) => Response::json(200, json!({"snapshots":snapshots})),
                Err(error) => snapshot_error(error),
            };
        }
        if let ["api", "snapshots", "uploads", upload, "commit"] = parts.as_slice() {
            return match store
                .snapshots
                .as_mut()
                .ok_or("Snapshot storage is unavailable".into())
                .and_then(|snapshots| snapshots.commit(principal, upload, now()))
            {
                Ok(summary) => Response::json(200, json!(summary)),
                Err(error) => snapshot_error(error),
            };
        }
        if let ["api", "snapshots", id, "delete"] = parts.as_slice() {
            return match delete_snapshot(store, principal, id) {
                Ok(value) => Response::json(200, value),
                Err(error) => snapshot_error(error),
            };
        }
    }
    Response::error(404, "Snapshot endpoint not found")
}
fn worker_snapshot(store: &mut Store, worker_id: &str, req: &Request) -> Response {
    let parts: Vec<_> = req.path.trim_start_matches('/').split('/').collect();
    let Some(job_id) = parts.get(3) else {
        return Response::error(404, "Job not found");
    };
    let Some(job) = store
        .state
        .jobs
        .iter()
        .find(|job| job["id"] == **job_id && job["workerId"] == worker_id)
    else {
        return Response::error(404, "Job not found");
    };
    if !["queued", "running"].contains(&job["status"].as_str().unwrap_or(""))
        || (job["status"] == "running"
            && now().saturating_sub(job["leaseAt"].as_u64().unwrap_or(0)) > 90_000)
    {
        return Response::error(
            409,
            "Snapshot is unavailable for a cancelled, expired, or terminal job",
        );
    }
    let (Some(owner), Some(id)) = (job["snapshotOwner"].as_str(), job["snapshotId"].as_str())
    else {
        return Response::error(404, "Job has no snapshot");
    };
    let Some(snapshots) = store.snapshots.as_mut() else {
        return Response::error(503, "Snapshot storage is unavailable");
    };
    match parts.as_slice() {
        ["api", "worker", "jobs", _, "snapshot"] => {
            match snapshots.owned_manifest(owner, id, now()) {
                Ok(manifest) => Response::json(200, json!(manifest)),
                Err(error) => snapshot_error(error),
            }
        }
        ["api", "worker", "jobs", _, "snapshot", "files", file, "chunks", chunk] => {
            let (Ok(file), Ok(chunk)) = (file.parse::<usize>(), chunk.parse::<usize>()) else {
                return Response::error(400, "Invalid snapshot file or chunk index");
            };
            match snapshots.read_chunk(owner, id, file, chunk, now()) {
                Ok(bytes) => Response::bytes("application/octet-stream", bytes),
                Err(error) => snapshot_error(error),
            }
        }
        _ => Response::error(404, "Worker snapshot endpoint not found"),
    }
}
fn worker_capabilities(value: &Value) -> Result<Option<Value>, String> {
    if value.is_null() {
        return Ok(None);
    }
    let version = value["version"].as_str().ok_or("Worker version required")?;
    if version.is_empty()
        || version.len() > 32
        || !version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-+".contains(&b))
    {
        return Err("Invalid worker version".into());
    }
    let docker = &value["docker"];
    let available = docker["available"]
        .as_bool()
        .ok_or("Docker availability must be boolean")?;
    let checked = docker
        .get("checked")
        .map(|v| v.as_bool().ok_or("Docker check status must be boolean"))
        .transpose()?
        .unwrap_or(true);
    let ready = docker["ready"]
        .as_bool()
        .ok_or("Docker readiness must be boolean")?;
    let message = docker["message"].as_str().unwrap_or("");
    if message.len() > 500
        || message.chars().any(char::is_control)
        || (ready && (!available || !checked))
    {
        return Err("Invalid Docker capability report".into());
    }
    Ok(Some(
        json!({"version":version,"docker":{"available":available,"ready":ready,"checked":checked,"message":message}}),
    ))
}

fn worker_api(store: &mut Store, worker_id: &str, req: &Request, b: &Value) -> Response {
    if req.method == "GET" && req.path.starts_with("/api/worker/jobs/") {
        return worker_snapshot(store, worker_id, req);
    }
    if req.method != "POST" {
        return Response::error(405, "POST required");
    }
    if req.path == "/api/worker/status" {
        let worker = store
            .state
            .workers
            .iter()
            .find(|w| w["id"] == worker_id)
            .unwrap();
        let running = store.state.jobs.iter().find(|j| {
            j["workerId"] == worker_id
                && matches!(j["status"].as_str(), Some("running" | "cancelling"))
        });
        return Response::json(
            200,
            json!({"workerId":worker_id,"name":worker["name"],"lastSeen":worker["lastSeen"],"lastPollAt":worker["lastPollAt"],"tasks":worker["tasks"],"runningJobId":running.map(|j| &j["id"]),"masterVersion":env!("CARGO_PKG_VERSION")}),
        );
    }
    if req.path == "/api/worker/poll" {
        let capabilities = match worker_capabilities(&b["capabilities"]) {
            Ok(value) => value,
            Err(error) => return Response::error(400, &error),
        };
        let refreshed = if b.get("tasks").is_some() {
            match tasks(&b["tasks"]) {
                Ok(t) => Some(t),
                Err(e) => return Response::error(400, &e),
            }
        } else {
            None
        };
        let w = store
            .state
            .workers
            .iter_mut()
            .find(|w| w["id"] == worker_id)
            .unwrap();
        w["lastSeen"] = json!(now());
        w["lastPollAt"] = w["lastSeen"].clone();
        if let Some(capabilities) = capabilities {
            w["capabilities"] = capabilities;
        }
        if let Some(t) = refreshed {
            w["tasks"] = t;
        }
        let current = b["runningJobId"].as_str();
        if let Some(id) = current {
            let j = store
                .state
                .jobs
                .iter_mut()
                .find(|j| j["id"] == id && j["workerId"] == worker_id);
            let stop = j.is_none_or(|j| {
                if j["status"] == "running" || j["status"] == "cancelling" {
                    j["leaseAt"] = json!(now());
                }
                j["status"] != "running"
            });
            return Response::json(200, json!({"job":null,"cancel":stop}));
        }
        // No re-delivery: uncertain execution must be reconciled manually, never executed twice.
        if store.state.jobs.iter().any(|j| {
            j["workerId"] == worker_id
                && ["running", "cancelling"].contains(&j["status"].as_str().unwrap_or(""))
        }) {
            return Response::json(200, json!({"job":null,"cancel":false}));
        }
        if let Some(j) = store
            .state
            .jobs
            .iter_mut()
            .find(|j| j["workerId"] == worker_id && j["status"] == "queued")
        {
            j["status"] = json!("running");
            j["updatedAt"] = json!(now());
            j["leaseAt"] = json!(now());
            return Response::json(
                200,
                json!({"job":{"id":j["id"],"task":j["task"],"snapshotId":j["snapshotId"]},"cancel":false}),
            );
        }
        return Response::json(200, json!({"job":null,"cancel":false}));
    }
    if let Some(id) = req
        .path
        .strip_prefix("/api/worker/jobs/")
        .and_then(|s| s.strip_suffix("/events"))
    {
        if let Some(w) = store
            .state
            .workers
            .iter_mut()
            .find(|w| w["id"] == worker_id)
        {
            w["lastSeen"] = json!(now());
        }
        let Some(j) = store
            .state
            .jobs
            .iter_mut()
            .find(|j| j["id"] == id && j["workerId"] == worker_id)
        else {
            return Response::error(404, "Job not found");
        };
        let Some(seq) = b["sequence"].as_u64() else {
            return Response::error(400, "sequence required");
        };
        let previous = j["sequence"].as_u64().unwrap_or(0);
        if seq <= previous {
            return Response::json(200, json!({"ok":true,"duplicate":true}));
        }
        if seq != previous + 1 {
            return Response::error(409, "Out-of-order event");
        }
        if !["running", "cancelling"].contains(&j["status"].as_str().unwrap_or("")) {
            return Response::json(
                200,
                json!({"ok":true,"ignored":true,"reason":"Job already terminal"}),
            );
        }
        if let Some(status) = b["status"].as_str() {
            if !["running", "passed", "failed", "cancelled"].contains(&status) {
                return Response::error(400, "Invalid job status");
            }
            if status != "running" {
                j["status"] = json!(if j["status"] == "cancelling" {
                    "cancelled"
                } else {
                    status
                });
                j["exitCode"] = b["exitCode"].clone();
            }
        }
        if let Some(log) = b["log"].as_str() {
            let mut full = j["log"].as_str().unwrap_or("").to_string();
            let limit = 256 * 1024;
            let remaining = limit - full.len().min(limit);
            let mut end = log.len().min(remaining);
            while !log.is_char_boundary(end) {
                end -= 1;
            }
            full.push_str(&log[..end]);
            j["log"] = json!(full);
            if end < log.len() {
                j["logTruncated"] = json!(true)
            }
        }
        j["sequence"] = json!(seq);
        j["updatedAt"] = json!(now());
        j["leaseAt"] = json!(now());
        return Response::json(200, json!({"ok":true}));
    }
    Response::error(404, "Unknown worker endpoint")
}
fn mcp(store: &mut Store, principal: &str, req: &Request, b: &Value) -> Response {
    if req.method != "POST" {
        return Response::error(405, "Stateless MCP supports POST only");
    }
    let id = b.get("id").cloned();
    let method = b["method"].as_str().unwrap_or("");
    if b["jsonrpc"] != "2.0" {
        return Response::json(
            200,
            json!({"jsonrpc":"2.0","id":id,"error":{"code":-32600,"message":"Invalid request"}}),
        );
    }
    if id.is_none() {
        return if method.starts_with("notifications/") {
            Response {
                status: 202,
                content_type: "application/json",
                body: vec![],
            }
        } else {
            Response::error(400, "id required")
        };
    }
    let result = match method {
        "initialize" => {
            let v = b["params"]["protocolVersion"].as_str().unwrap_or("");
            Ok(
                json!({"protocolVersion":if ["2025-03-26","2025-06-18","2025-11-25"].contains(&v){v}else{"2025-03-26"},"capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"sharebox","version":env!("CARGO_PKG_VERSION")},"instructions":"Only owner-registered task names can execute. Local tasks use existing worker checkouts. Upload source with the local sharebox snapshot upload CLI, then submit a committed snapshotId accessible to this connection. MCP cannot read client-side files itself. Inspect list_tasks for snapshot and execution descriptors."}),
            )
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools":tool_list()})),
        "tools/call" => {
            let name = b["params"]["name"].as_str().unwrap_or("");
            let a = &b["params"]["arguments"];
            let output:Result<Value,String>=match name{
            "list_workers"=>Ok(json!({"workers":store.state.workers.iter().filter(|w|!w["revoked"].as_bool().unwrap_or(false)).map(clean).collect::<Vec<_>>()})),
            "list_tasks"=>Ok(json!({"workers":store.state.workers.iter().filter(|w|!w["revoked"].as_bool().unwrap_or(false)).map(|w|json!({"workerId":w["id"],"name":w["name"],"tasks":w["tasks"]})).collect::<Vec<_>>()})),
            "submit_job"=>submit(store,principal,a),"cancel_job"=>cancel(&mut store.state,a["jobId"].as_str().unwrap_or("")),
            "get_job"|"get_job_logs"=>store.state.jobs.iter().find(|j|j["id"]==a["jobId"]).map(|j|if name=="get_job_logs"{let log=j["log"].as_str().unwrap_or("");let requested=a["offset"].as_u64().unwrap_or(0);let mut offset=(requested.min(log.len() as u64))as usize;while !log.is_char_boundary(offset){offset-=1}json!({"jobId":j["id"],"status":j["status"],"log":&log[offset..],"nextOffset":log.len(),"truncated":j["logTruncated"].as_bool().unwrap_or(false)})}else{let mut out=clean(j);out.as_object_mut().unwrap().remove("log");out}).ok_or("Job not found".into()),
            "list_snapshots"=>store.snapshots.as_mut().ok_or("Snapshot storage is unavailable".into()).and_then(|snapshots| snapshots.list(principal,now())).map(|snapshots|json!({"snapshots":snapshots})),
            "delete_snapshot"=>delete_snapshot(store,principal,a["snapshotId"].as_str().unwrap_or("")),
            _=>Err("Unknown tool".into())};
            match output {
                Ok(v) => {
                    Ok(json!({"content":[{"type":"text","text":v.to_string()}],"isError":false}))
                }
                Err(e) => Ok(json!({"content":[{"type":"text","text":e}],"isError":true})),
            }
        }
        _ => Err("Method not found"),
    };
    match result {
        Ok(r) => Response::json(200, json!({"jsonrpc":"2.0","id":id,"result":r})),
        Err(e) => Response::json(
            200,
            json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":e}}),
        ),
    }
}
fn tool_list() -> Vec<Value> {
    let definitions = [
        ("list_snapshots", "List accessible committed snapshots (agents see their own; admin sees all)", json!({}), vec![]),
        ("delete_snapshot", "Delete an owned snapshot when no active job references it", json!({"snapshotId":{"type":"string"}}), vec!["snapshotId"]),
        (
            "list_workers",
            "List registered machines and last-seen timestamps",
            json!({}),
            vec![],
        ),
        (
            "list_tasks",
            "List owner-permitted task names on workers",
            json!({}),
            vec![],
        ),
        (
            "submit_job",
            "Run an owner-registered task; snapshot tasks require an owned snapshotId; returns immediately",
            json!({"workerId":{"type":"string"},"task":{"type":"string"},"snapshotId":{"type":"string","description":"Required only for tasks advertising snapshot:true"},"requestKey":{"type":"string","description":"Optional unique idempotency key"}}),
            vec!["workerId", "task"],
        ),
        (
            "get_job",
            "Read job status and exit code",
            json!({"jobId":{"type":"string"}}),
            vec!["jobId"],
        ),
        (
            "get_job_logs",
            "Read logs using optional UTF-8 byte offset",
            json!({"jobId":{"type":"string"},"offset":{"type":"integer","minimum":0}}),
            vec!["jobId"],
        ),
        (
            "cancel_job",
            "Cancel a queued job or request running process-tree termination",
            json!({"jobId":{"type":"string"}}),
            vec!["jobId"],
        ),
    ];
    definitions.into_iter().map(|(name,description,properties,required)|json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false}})).collect()
}
fn handle(store: &mut Store, req: &Request) -> Response {
    let host = req.headers.get("host").map(String::as_str).unwrap_or("");
    let hostname = host.split(':').next().unwrap_or("");
    if hostname != "localhost" && hostname.parse::<IpAddr>().is_err() {
        return Response::error(403, "Use localhost or the master's LAN IP address");
    }
    if let Some(origin) = req.headers.get("origin") {
        if origin != &format!("http://{host}") {
            return Response::error(403, "Cross-origin request rejected");
        }
    }
    if req.method == "GET" {
        match req.path.as_str() {
            "/" | "/index.html" => {
                return Response::bytes(
                    "text/html; charset=utf-8",
                    include_bytes!("../public/index.html").to_vec(),
                )
            }
            "/app.js" => {
                return Response::bytes(
                    "text/javascript; charset=utf-8",
                    include_bytes!("../public/app.js").to_vec(),
                )
            }
            "/style.css" => {
                return Response::bytes(
                    "text/css; charset=utf-8",
                    include_bytes!("../public/style.css").to_vec(),
                )
            }
            "/health" => {
                return Response::json(200, json!({"ok":true,"version":env!("CARGO_PKG_VERSION")}))
            }
            _ => {}
        }
    }
    let role = store.role(req);
    if req.path.starts_with("/api/snapshots/") {
        let principal = match &role {
            Role::Admin => "admin",
            Role::Agent(id) => id,
            Role::None => return Response::error(401, "Valid bearer token required"),
            _ => return Response::error(403, "Worker tokens cannot manage snapshots"),
        };
        return snapshot_api(store, principal, req);
    }
    let body: Value = if req.body.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(_) => return Response::error(400, "Invalid JSON"),
        }
    };
    if req.path == "/api/worker/pair" && req.method == "POST" {
        let Some(code) = body["code"].as_str() else {
            return Response::error(400, "code required");
        };
        let Some(i) = store.state.pairings.iter().position(|p| {
            p["expiresAt"].as_u64().unwrap_or(0) > now()
                && equal(p["code"].as_str().unwrap_or(""), code)
        }) else {
            return Response::error(401, "Invalid or expired pairing code");
        };
        let t = match tasks(&body["tasks"]) {
            Ok(t) => t,
            Err(e) => return Response::error(400, &e),
        };
        let capabilities = match worker_capabilities(&body["capabilities"]) {
            Ok(value) => value,
            Err(error) => return Response::error(400, &error),
        };
        let name = body["name"].as_str().unwrap_or("Worker");
        if name.len() > 100 {
            return Response::error(400, "Worker name too long");
        }
        if store.state.workers.len() >= 64 {
            return Response::error(409, "Worker limit reached");
        }
        store.state.pairings.remove(i);
        let token = random();
        let id = format!("worker-{}", &random()[..16]);
        store.state.workers.push(json!({"id":id,"name":name,"os":body["os"],"arch":body["arch"],"tasks":t,"capabilities":capabilities,"token":token,"lastSeen":now(),"lastPollAt":0,"revoked":false}));
        return Response::json(201, json!({"token":token,"workerId":id}));
    }
    if role == Role::None {
        return Response::error(401, "Valid bearer token required");
    }
    if let Role::Worker(id) = &role {
        return worker_api(store, id, req, &body);
    }
    if req.path == "/mcp" {
        let principal = match &role {
            Role::Admin => "admin",
            Role::Agent(id) => id,
            _ => return Response::error(403, "Agent token required"),
        };
        return mcp(store, principal, req, &body);
    }
    if role != Role::Admin {
        return Response::error(403, "Admin token required");
    }
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/api/network") => {
            Response::json(200, json!(crate::network::snapshot(store.bound_addr)))
        }
        ("GET", "/api/me") => Response::json(
            200,
            json!({"name":"Local workspace","version":env!("CARGO_PKG_VERSION")}),
        ),
        ("GET", "/api/workspace") => Response::json(
            200,
            json!({"workers":store.state.workers.iter().map(clean).collect::<Vec<_>>(),"jobs":store.state.jobs.iter().rev().map(|j|{let mut out=clean(j);out.as_object_mut().unwrap().remove("log");out}).collect::<Vec<_>>(),"tokens":store.state.tokens.iter().map(clean).collect::<Vec<_>>()}),
        ),
        ("POST", "/api/pairing") => {
            if store.state.pairings.len() > 20 {
                return Response::error(429, "Too many active pairing codes");
            }
            let p = json!({"code":random(),"expiresAt":now()+600_000});
            store.state.pairings.push(p.clone());
            Response::json(201, p)
        }
        ("POST", "/api/tokens") => {
            if store.state.tokens.len() >= 64 {
                return Response::error(409, "Token limit reached");
            }
            let name = body["name"].as_str().unwrap_or("LLM agent");
            if name.len() > 100 {
                return Response::error(400, "Name too long");
            }
            let t = json!({"id":format!("token-{}",&random()[..16]),"name":name,"token":random(),"createdAt":now()});
            store.state.tokens.push(t.clone());
            Response::json(201, t)
        }
        ("POST", "/api/jobs") => match submit(store, "admin", &body) {
            Ok(j) => Response::json(201, j),
            Err(e) => Response::error(400, &e),
        },
        _ => {
            if let Some(id) = req.path.strip_prefix("/api/jobs/") {
                if let Some(id) = id.strip_suffix("/cancel") {
                    if req.method == "POST" {
                        return match cancel(&mut store.state, id) {
                            Ok(j) => Response::json(200, j),
                            Err(e) => Response::error(404, &e),
                        };
                    }
                } else if req.method == "GET" {
                    return store
                        .state
                        .jobs
                        .iter()
                        .find(|j| j["id"] == id)
                        .map(|j| Response::json(200, clean(j)))
                        .unwrap_or_else(|| Response::error(404, "Job not found"));
                }
            }
            if req.method == "DELETE" {
                if let Some(id) = req.path.strip_prefix("/api/tokens/") {
                    store.state.tokens.retain(|t| t["id"] != id);
                    return Response::json(200, json!({"ok":true}));
                }
            }
            if req.method == "POST" {
                if let Some(id) = req
                    .path
                    .strip_prefix("/api/workers/")
                    .and_then(|s| s.strip_suffix("/revoke"))
                {
                    if let Some(w) = store.state.workers.iter_mut().find(|w| w["id"] == id) {
                        w["revoked"] = json!(true);
                        for j in &mut store.state.jobs {
                            if j["workerId"] == id
                                && ["queued", "running", "cancelling"]
                                    .contains(&j["status"].as_str().unwrap_or(""))
                            {
                                j["status"] = json!("cancelled");
                                j["updatedAt"] = json!(now());
                            }
                        }
                        return Response::json(200, json!({"ok":true}));
                    }
                    return Response::error(404, "Worker not found");
                }
            }
            Response::error(404, "Endpoint not found")
        }
    }
}
pub fn run(bind: &str, dir: PathBuf) -> Result<(), String> {
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    }
    let mut lock_options = fs::OpenOptions::new();
    lock_options
        .read(true)
        .write(true)
        .create(true)
        .truncate(false);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        lock_options.share_mode(0);
    }
    let lock = lock_options
        .open(dir.join("master.lock"))
        .map_err(|e| format!("Cannot lock data directory: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("Data directory already used by another master".into());
        }
    }
    let token_path = dir.join("admin-token");
    let admin = if token_path.exists() {
        fs::read_to_string(&token_path)
            .map_err(|e| e.to_string())?
            .trim()
            .to_string()
    } else {
        let t = random();
        private_write(&token_path, t.as_bytes())?;
        t
    };
    if admin.len() != 64 {
        return Err("Invalid admin-token file".into());
    }
    let path = dir.join("state.json");
    let state = if path.exists() {
        serde_json::from_slice(&fs::read(&path).map_err(|e| e.to_string())?)
            .map_err(|e| format!("Cannot read state; refusing to overwrite: {e}"))?
    } else {
        State::default()
    };
    let listener = TcpListener::bind(bind).map_err(|e| e.to_string())?;
    let bound_addr = listener.local_addr().map_err(|e| e.to_string())?;
    let store = Arc::new(Mutex::new(Store {
        state,
        path,
        admin: admin.clone(),
        bound_addr,
        snapshots: Some(SnapshotStore::open(dir.join("snapshots"))?),
    }));
    store.lock().unwrap().save()?;
    println!("Sharebox master listening on http://{}\nDashboard: http://localhost:{}\nAdmin token: {admin}\nSaved at {}\nTrusted private LAN only: HTTP is unencrypted. Native tasks use worker account permissions.",bound_addr,bound_addr.port(),token_path.display());
    let active = Arc::new(AtomicUsize::new(0));
    for socket in listener.incoming() {
        let Ok(mut socket) = socket else { continue };
        if active.fetch_add(1, Ordering::SeqCst) >= 64 {
            active.fetch_sub(1, Ordering::SeqCst);
            http::write_response(
                &mut socket,
                Response::error(429, "Server connection limit reached"),
            );
            continue;
        }
        let store = store.clone();
        let active = active.clone();
        std::thread::spawn(move || {
            let response = match http::read_request(&mut socket) {
                Ok(req) => {
                    let mut guard = store.lock().unwrap();
                    if req.path.starts_with("/api/snapshots/")
                        || (req.method == "GET" && req.path.starts_with("/api/worker/jobs/"))
                    {
                        let response = handle(&mut guard, &req);
                        drop(guard);
                        http::write_response(&mut socket, response);
                        active.fetch_sub(1, Ordering::SeqCst);
                        return;
                    }
                    let before = guard.state.clone();
                    guard.expire();
                    let res = handle(&mut guard, &req);
                    if serde_json::to_value(&before).unwrap()
                        != serde_json::to_value(&guard.state).unwrap()
                    {
                        if let Err(e) = guard.save() {
                            guard.state = before;
                            eprintln!("Cannot persist state: {e}");
                            Response::error(500, "Unable to persist state; operation rolled back")
                        } else {
                            res
                        }
                    } else {
                        res
                    }
                }
                Err(e) => Response::error(400, &e),
            };
            http::write_response(&mut socket, response);
            active.fetch_sub(1, Ordering::SeqCst);
        });
    }
    drop(lock);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    fn req(method: &str, path: &str, token: &str, b: Value) -> Request {
        Request {
            method: method.into(),
            path: path.into(),
            headers: HashMap::from([
                ("host".into(), "localhost:7331".into()),
                ("authorization".into(), format!("Bearer {token}")),
            ]),
            body: serde_json::to_vec(&b).unwrap(),
        }
    }
    fn store() -> Store {
        Store {
            state: State::default(),
            path: PathBuf::new(),
            admin: "admin".into(),
            bound_addr: "127.0.0.1:7331".parse().unwrap(),
            snapshots: None,
        }
    }
    struct SnapshotFixture(PathBuf);
    impl Drop for SnapshotFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn snapshot_store() -> (SnapshotFixture, Store) {
        let directory = std::env::temp_dir().join(format!("sharebox-master-snapshot-{}", random()));
        let mut store = store();
        store.snapshots = Some(SnapshotStore::open(directory.clone()).unwrap());
        store.state.tokens = vec![
            json!({"id":"owner-a","token":"agent-a"}),
            json!({"id":"owner-b","token":"agent-b"}),
        ];
        store.state.workers = vec![
            json!({"id":"worker-a","token":"worker-a","lastSeen":now(),"tasks":[{"name":"source-test","snapshot":true,"execution":"native"},{"name":"local-test"}]}),
            json!({"id":"worker-b","token":"worker-b","lastSeen":now(),"tasks":[]}),
        ];
        (SnapshotFixture(directory), store)
    }
    fn snapshot_manifest(bytes: &[u8]) -> SnapshotManifest {
        SnapshotManifest {
            version: 1,
            files: vec![sharebox::snapshot::SnapshotFile {
                path: "src/test.js".into(),
                size: bytes.len() as u64,
                sha256: sharebox::snapshot::hash_bytes(bytes),
                executable: false,
            }],
        }
    }
    fn committed_snapshot(store: &mut Store, bytes: &[u8]) -> String {
        let snapshots = store.snapshots.as_mut().unwrap();
        let upload = snapshots
            .begin("owner-a", &snapshot_manifest(bytes), now())
            .unwrap();
        snapshots
            .chunk("owner-a", &upload, 0, 0, bytes, now())
            .unwrap();
        snapshots
            .commit("owner-a", &upload, now())
            .unwrap()
            .snapshot_id
    }
    #[test]
    fn snapshot_upload_http_auth_binary_chunks_and_token_revocation() {
        let (_fixture, mut store) = snapshot_store();
        let bytes = [0xff, 0, 1, 2];
        let body = json!(snapshot_manifest(&bytes));
        assert_eq!(
            handle(
                &mut store,
                &req("POST", "/api/snapshots/uploads", "", body.clone())
            )
            .status,
            401
        );
        assert_eq!(
            handle(
                &mut store,
                &req("POST", "/api/snapshots/uploads", "worker-a", body.clone())
            )
            .status,
            403
        );
        let response = handle(
            &mut store,
            &req("POST", "/api/snapshots/uploads", "agent-a", body),
        );
        assert_eq!(response.status, 201);
        let upload: Value = serde_json::from_slice(&response.body).unwrap();
        let path = format!(
            "/api/snapshots/uploads/{}/files/0/chunks/0",
            upload["uploadId"].as_str().unwrap()
        );
        let mut chunk = req("PUT", &path, "agent-b", json!({}));
        chunk.body = bytes.to_vec();
        assert_eq!(handle(&mut store, &chunk).status, 404);
        chunk
            .headers
            .insert("authorization".into(), "Bearer agent-a".into());
        assert_eq!(handle(&mut store, &chunk).status, 200);
        let commit_path = format!(
            "/api/snapshots/uploads/{}/commit",
            upload["uploadId"].as_str().unwrap()
        );
        assert_eq!(
            handle(&mut store, &req("POST", &commit_path, "agent-a", json!({}))).status,
            200
        );
        store.state.tokens.retain(|token| token["id"] != "owner-a");
        assert_eq!(handle(&mut store, &chunk).status, 401);
        assert_eq!(
            handle(&mut store, &req("POST", &commit_path, "agent-a", json!({}))).status,
            401
        );
    }
    #[test]
    fn snapshot_jobs_require_task_optin_owner_access_and_exact_idempotency() {
        let (_fixture, mut store) = snapshot_store();
        let first = committed_snapshot(&mut store, b"first");
        let second = committed_snapshot(&mut store, b"second");
        let args = json!({"workerId":"worker-a","task":"source-test","snapshotId":first,"requestKey":"same-job"});
        assert!(submit(&mut store, "owner-b", &args).is_err());
        assert!(submit(
            &mut store,
            "owner-a",
            &json!({"workerId":"worker-a","task":"source-test"})
        )
        .is_err());
        assert!(submit(
            &mut store,
            "owner-a",
            &json!({"workerId":"worker-a","task":"local-test","snapshotId":first})
        )
        .is_err());
        let job = submit(&mut store, "owner-a", &args).unwrap();
        assert_eq!(job["source"], "snapshot");
        assert_eq!(job["execution"], "native");
        assert_eq!(
            submit(&mut store, "owner-a", &args).unwrap()["id"],
            job["id"]
        );
        let mut changed = args.clone();
        changed["snapshotId"] = json!(second);
        assert!(submit(&mut store, "owner-a", &changed).is_err());
        assert!(delete_snapshot(&mut store, "admin", &first).is_err());
        let admin_job = submit(
            &mut store,
            "admin",
            &json!({"workerId":"worker-a","task":"source-test","snapshotId":first}),
        )
        .unwrap();
        assert!(store
            .state
            .jobs
            .iter()
            .any(|job| job["id"] == admin_job["id"] && job["snapshotOwner"] == "owner-a"));
        for job in &mut store.state.jobs {
            job["status"] = json!("failed");
        }
        assert!(delete_snapshot(&mut store, "admin", &first).is_ok());
        assert!(store
            .snapshots
            .as_mut()
            .unwrap()
            .owned_manifest("owner-a", &first, now())
            .is_err());
    }
    #[test]
    fn worker_snapshot_download_is_job_bound_and_stops_on_cancel_lease_or_revocation() {
        let (_fixture, mut store) = snapshot_store();
        let id = committed_snapshot(&mut store, b"source");
        let job = submit(
            &mut store,
            "owner-a",
            &json!({"workerId":"worker-a","task":"source-test","snapshotId":id}),
        )
        .unwrap();
        let path = format!("/api/worker/jobs/{}/snapshot", job["id"].as_str().unwrap());
        assert_eq!(
            handle(&mut store, &req("GET", &path, "worker-b", json!({}))).status,
            404
        );
        assert_eq!(
            handle(&mut store, &req("GET", &path, "worker-a", json!({}))).status,
            200
        );
        let chunk_path = format!("{path}/files/0/chunks/0");
        let chunk = handle(&mut store, &req("GET", &chunk_path, "worker-a", json!({})));
        assert_eq!(chunk.status, 200);
        assert_eq!(chunk.body, b"source");
        store.state.jobs[0]["status"] = json!("cancelling");
        assert_eq!(
            handle(&mut store, &req("GET", &chunk_path, "worker-a", json!({}))).status,
            409
        );
        store.state.jobs[0]["status"] = json!("running");
        store.state.jobs[0]["leaseAt"] = json!(now() - 91_000);
        assert_eq!(
            handle(&mut store, &req("GET", &path, "worker-a", json!({}))).status,
            409
        );
        store.state.jobs[0]["leaseAt"] = json!(now());
        store.state.workers[0]["revoked"] = json!(true);
        assert_eq!(
            handle(&mut store, &req("GET", &path, "worker-a", json!({}))).status,
            401
        );
    }
    #[test]
    fn network_info_requires_admin_and_uses_listener_address() {
        let mut s = store();
        s.bound_addr = "127.0.0.1:49152".parse().unwrap();
        s.state
            .tokens
            .push(json!({"id":"token-agent","token":"agent"}));
        assert_eq!(
            handle(&mut s, &req("GET", "/api/network", "", json!({}))).status,
            401
        );
        assert_eq!(
            handle(&mut s, &req("GET", "/api/network", "agent", json!({}))).status,
            403
        );
        let response = handle(&mut s, &req("GET", "/api/network", "admin", json!({})));
        assert_eq!(response.status, 200);
        let body: Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(
            body,
            json!({"bindAddress":"127.0.0.1:49152", "localOnly":true, "urls":[]})
        );
    }
    #[test]
    fn pairing_is_single_use() {
        let mut s = store();
        let p = handle(&mut s, &req("POST", "/api/pairing", "admin", json!({})));
        let code: Value = serde_json::from_slice(&p.body).unwrap();
        let b = json!({"code":code["code"],"tasks":[{"name":"test"}]});
        assert_eq!(
            handle(&mut s, &req("POST", "/api/worker/pair", "", b.clone())).status,
            201
        );
        assert_eq!(
            handle(&mut s, &req("POST", "/api/worker/pair", "", b)).status,
            401
        );
    }
    #[test]
    fn agent_cannot_admin() {
        let mut s = store();
        s.state
            .tokens
            .push(json!({"id":"token-agent","token":"agent"}));
        assert_eq!(
            handle(&mut s, &req("POST", "/api/pairing", "agent", json!({}))).status,
            403
        );
        assert_eq!(
            handle(
                &mut s,
                &req(
                    "POST",
                    "/mcp",
                    "agent",
                    json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})
                )
            )
            .status,
            200
        );
    }
    #[test]
    fn rejects_cross_origin() {
        let mut r = req("GET", "/api/me", "admin", json!({}));
        r.headers
            .insert("origin".into(), "http://evil.example".into());
        assert_eq!(handle(&mut store(), &r).status, 403);
    }
    #[test]
    fn status_is_authenticated_read_only_and_never_claims_jobs() {
        let mut s = store();
        s.state.workers.push(json!({"id":"w","token":"worker","name":"My worker","lastSeen":123,"lastPollAt":0,"tasks":[{"name":"test"}]}));
        s.state
            .jobs
            .push(json!({"id":"queued","workerId":"w","task":"test","status":"queued"}));
        let workers = s.state.workers.clone();
        let jobs = s.state.jobs.clone();
        let response = handle(
            &mut s,
            &req(
                "POST",
                "/api/worker/status",
                "worker",
                json!({"workerId":"someone-else"}),
            ),
        );
        assert_eq!(response.status, 200);
        let body: Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(body["workerId"], "w");
        assert!(body.get("token").is_none());
        assert!(body["runningJobId"].is_null());
        assert_eq!(s.state.workers, workers);
        assert_eq!(s.state.jobs, jobs);
        s.state.workers[0]["revoked"] = json!(true);
        assert_eq!(
            handle(
                &mut s,
                &req("POST", "/api/worker/status", "worker", json!({}))
            )
            .status,
            401
        );
    }
    #[test]
    fn capabilities_are_bounded_sanitized_and_heartbeat_backwards_compatible() {
        assert!(worker_capabilities(&Value::Null).unwrap().is_none());
        let report = json!({"version":"0.6.0","docker":{"available":true,"ready":true,"message":"Docker ready","secret":"excluded"},"credentials":"excluded"});
        let clean = worker_capabilities(&report).unwrap().unwrap();
        assert!(clean.get("credentials").is_none());
        assert!(clean["docker"].get("secret").is_none());
        for invalid in [
            json!({"version":"x","docker":{"available":false,"ready":true}}),
            json!({"version":"x","docker":{"available":true,"ready":true,"message":"x".repeat(501)}}),
        ] {
            assert!(worker_capabilities(&invalid).is_err());
        }
        let mut s = store();
        s.state.workers.push(json!({"id":"w","token":"worker","lastSeen":0,"lastPollAt":0,"tasks":[{"name":"test"}]}));
        assert_eq!(
            handle(
                &mut s,
                &req(
                    "POST",
                    "/api/worker/poll",
                    "worker",
                    json!({"capabilities":report})
                )
            )
            .status,
            200
        );
        assert_eq!(s.state.workers[0]["capabilities"], clean);
        assert!(s.state.workers[0]["lastPollAt"].as_u64().unwrap() > 0);
        assert_eq!(
            handle(
                &mut s,
                &req("POST", "/api/worker/poll", "worker", json!({}))
            )
            .status,
            200
        );
    }

    #[test]
    fn cancelled_queue_is_never_assigned() {
        let mut s = store();
        s.state
            .workers
            .push(json!({"id":"w","token":"worker","lastSeen":now(),"tasks":[{"name":"test"}]}));
        let j = submit(&mut s, "admin", &json!({"workerId":"w","task":"test"})).unwrap();
        cancel(&mut s.state, j["id"].as_str().unwrap()).unwrap();
        let r = handle(
            &mut s,
            &req("POST", "/api/worker/poll", "worker", json!({})),
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&r.body).unwrap()["job"],
            Value::Null
        );
    }
    #[test]
    fn idle_heartbeats_do_not_extend_lost_assignment() {
        let mut s = store();
        s.state
            .workers
            .push(json!({"id":"w","token":"worker","lastSeen":now()}));
        s.state.jobs.push(
            json!({"id":"job-a","workerId":"w","status":"running","leaseAt":now()-91_000,"log":""}),
        );
        s.expire();
        assert_eq!(s.state.jobs[0]["status"], "lost");
        let response = worker_api(
            &mut s,
            "w",
            &req("POST", "/api/worker/jobs/job-a/events", "worker", json!({})),
            &json!({"sequence":1,"status":"passed"}),
        );
        assert_eq!(response.status, 200);
        assert_eq!(s.state.jobs[0]["status"], "lost");
    }
    #[test]
    fn duplicate_events_do_not_duplicate_logs() {
        let mut s = store();
        s.state
            .workers
            .push(json!({"id":"w","token":"worker","lastSeen":now()}));
        s.state
            .jobs
            .push(json!({"id":"job-a","workerId":"w","status":"running","sequence":0,"log":""}));
        let r = req("POST", "/api/worker/jobs/job-a/events", "worker", json!({}));
        let body = json!({"sequence":1,"log":"only once"});
        assert_eq!(worker_api(&mut s, "w", &r, &body).status, 200);
        assert_eq!(worker_api(&mut s, "w", &r, &body).status, 200);
        assert_eq!(s.state.jobs[0]["log"], "only once");
        assert_eq!(
            worker_api(&mut s, "w", &r, &json!({"sequence":3})).status,
            409
        );
    }
}
