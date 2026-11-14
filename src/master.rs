use crate::http::{self, Request, Response};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    net::{IpAddr, TcpListener},
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
        if self
            .state
            .tokens
            .iter()
            .any(|x| equal(t, x["token"].as_str().unwrap_or("")))
        {
            return Role::Agent;
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
    Agent,
    Worker(String),
    None,
}
fn clean(v: &Value) -> Value {
    let mut v = v.clone();
    if let Some(o) = v.as_object_mut() {
        o.remove("token");
        o.remove("requestKey");
        o.remove("sequence");
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
    Ok(v.clone())
}
fn submit(s: &mut State, b: &Value) -> Result<Value, String> {
    let worker_id = b["workerId"].as_str().ok_or("workerId required")?;
    let task = b["task"].as_str().ok_or("task required")?;
    let w = s
        .workers
        .iter()
        .find(|w| w["id"] == worker_id && !w["revoked"].as_bool().unwrap_or(false))
        .ok_or("Worker not found")?;
    if now().saturating_sub(w["lastSeen"].as_u64().unwrap_or(0)) > 30_000 {
        return Err("Worker is offline; start it before submitting".into());
    }
    if !w["tasks"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .any(|t| t["name"] == task)
    {
        return Err("Task is not permitted on this worker".into());
    }
    if let Some(key) = b["requestKey"].as_str() {
        if key.len() > 128 {
            return Err("requestKey too long".into());
        }
        if let Some(j) = s.jobs.iter().find(|j| j["requestKey"] == key) {
            if j["task"] != task || j["workerId"] != worker_id {
                return Err("requestKey already used for another task".into());
            }
            return Ok(clean(j));
        }
    }
    if s.jobs.len() >= 500 {
        return Err(
            "Local history limit reached (500 jobs); archive state before adding more".into(),
        );
    }
    let j = json!({"id":format!("job-{}",&random()[..16]),"workerId":worker_id,"task":task,"status":"queued","createdAt":now(),"updatedAt":now(),"log":"","sequence":0,"requestKey":b["requestKey"],"execution":"native","source":"worker-local-checkout"});
    s.jobs.push(j.clone());
    Ok(clean(&j))
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
fn worker_api(store: &mut Store, worker_id: &str, req: &Request, b: &Value) -> Response {
    if req.method != "POST" {
        return Response::error(405, "POST required");
    }
    if req.path == "/api/worker/poll" {
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
                json!({"job":{"id":j["id"],"task":j["task"]},"cancel":false}),
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
fn mcp(s: &mut State, req: &Request, b: &Value) -> Response {
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
                json!({"protocolVersion":if ["2025-03-26","2025-06-18","2025-11-25"].contains(&v){v}else{"2025-03-26"},"capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"sharebox","version":env!("CARGO_PKG_VERSION")},"instructions":"Tasks run in each worker's owner-configured local checkout. No source synchronization. Only registered task names can execute."}),
            )
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools":tool_list()})),
        "tools/call" => {
            let name = b["params"]["name"].as_str().unwrap_or("");
            let a = &b["params"]["arguments"];
            let output:Result<Value,String>=match name{
            "list_workers"=>Ok(json!({"workers":s.workers.iter().filter(|w|!w["revoked"].as_bool().unwrap_or(false)).map(clean).collect::<Vec<_>>()})),
            "list_tasks"=>Ok(json!({"workers":s.workers.iter().filter(|w|!w["revoked"].as_bool().unwrap_or(false)).map(|w|json!({"workerId":w["id"],"name":w["name"],"tasks":w["tasks"]})).collect::<Vec<_>>()})),
            "submit_job"=>submit(s,a),"cancel_job"=>cancel(s,a["jobId"].as_str().unwrap_or("")),
            "get_job"|"get_job_logs"=>s.jobs.iter().find(|j|j["id"]==a["jobId"]).map(|j|if name=="get_job_logs"{let log=j["log"].as_str().unwrap_or("");let requested=a["offset"].as_u64().unwrap_or(0);let mut offset=(requested.min(log.len() as u64))as usize;while !log.is_char_boundary(offset){offset-=1}json!({"jobId":j["id"],"status":j["status"],"log":&log[offset..],"nextOffset":log.len(),"truncated":j["logTruncated"].as_bool().unwrap_or(false)})}else{let mut out=clean(j);out.as_object_mut().unwrap().remove("log");out}).ok_or("Job not found".into()),
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
            "Run a named task on the worker's existing checkout; returns immediately",
            json!({"workerId":{"type":"string"},"task":{"type":"string"},"requestKey":{"type":"string","description":"Optional unique idempotency key"}}),
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
        store.state.workers.push(json!({"id":id,"name":name,"os":body["os"],"arch":body["arch"],"tasks":t,"token":token,"lastSeen":now(),"revoked":false}));
        return Response::json(201, json!({"token":token,"workerId":id}));
    }
    let role = store.role(req);
    if role == Role::None {
        return Response::error(401, "Valid bearer token required");
    }
    if let Role::Worker(id) = &role {
        return worker_api(store, id, req, &body);
    }
    if req.path == "/mcp" {
        return mcp(&mut store.state, req, &body);
    }
    if role != Role::Admin {
        return Response::error(403, "Admin token required");
    }
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/api/me") => Response::json(200, json!({"name":"Local workspace"})),
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
        ("POST", "/api/jobs") => match submit(&mut store.state, &body) {
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
    let store = Arc::new(Mutex::new(Store {
        state,
        path,
        admin: admin.clone(),
    }));
    store.lock().unwrap().save()?;
    let listener = TcpListener::bind(bind).map_err(|e| e.to_string())?;
    println!("Sharebox master listening on http://{}\nDashboard: http://localhost:{}\nAdmin token: {admin}\nSaved at {}\nTrusted private LAN only: HTTP is unencrypted. Native tasks use worker account permissions.",listener.local_addr().unwrap(),listener.local_addr().unwrap().port(),token_path.display());
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
        }
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
        s.state.tokens.push(json!({"token":"agent"}));
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
    fn cancelled_queue_is_never_assigned() {
        let mut s = store();
        s.state
            .workers
            .push(json!({"id":"w","token":"worker","lastSeen":now(),"tasks":[{"name":"test"}]}));
        let j = submit(&mut s.state, &json!({"workerId":"w","task":"test"})).unwrap();
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
