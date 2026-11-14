use serde_json::Value;
use std::{
    collections::HashMap,
    io::{self, Read, Write},
    net::TcpStream,
    time::{Duration, Instant},
};

pub const MAX_BODY: usize = 1024 * 1024;

// A socket timeout alone restarts after every successful read. Bound the whole
// message so a peer cannot retain a connection indefinitely by dripping bytes.
struct DeadlineReader<'a> {
    stream: &'a mut TcpStream,
    deadline: Instant,
}
impl<'a> DeadlineReader<'a> {
    fn new(stream: &'a mut TcpStream, budget: Duration) -> Self {
        Self {
            stream,
            deadline: Instant::now() + budget,
        }
    }
}
impl Read for DeadlineReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::TimedOut, "HTTP message deadline exceeded")
            })?;
        self.stream.set_read_timeout(Some(remaining))?;
        self.stream.read(buf)
    }
}
pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}
pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}
impl Response {
    pub fn json(status: u16, value: Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: serde_json::to_vec(&value).unwrap(),
        }
    }
    pub fn error(status: u16, message: &str) -> Self {
        Self::json(status, serde_json::json!({"error":message}))
    }
    pub fn bytes(content_type: &'static str, body: Vec<u8>) -> Self {
        Self {
            status: 200,
            content_type,
            body,
        }
    }
}
type HttpMessage = (String, HashMap<String, String>, Vec<u8>);
fn read_message(stream: &mut impl Read) -> Result<HttpMessage, String> {
    let mut data = Vec::new();
    let mut chunk = [0; 4096];
    let split = loop {
        let n = stream.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("Incomplete HTTP headers".into());
        }
        data.extend_from_slice(&chunk[..n]);
        if let Some(i) = data.windows(4).position(|v| v == b"\r\n\r\n") {
            if i > 16384 {
                return Err("Headers too large".into());
            }
            break i;
        }
        if data.len() > 16384 {
            return Err("Headers too large".into());
        }
    };
    let header = std::str::from_utf8(&data[..split]).map_err(|_| "Invalid header encoding")?;
    let mut lines = header.split("\r\n");
    let first = lines.next().unwrap().to_string();
    let mut headers = HashMap::new();
    for line in lines {
        let (k, v) = line.split_once(':').ok_or("Malformed header")?;
        let k = k.trim().to_lowercase();
        if headers.insert(k, v.trim().to_string()).is_some() {
            return Err("Duplicate headers rejected".into());
        }
    }
    if headers.contains_key("transfer-encoding") {
        return Err("Chunked bodies are not supported; send Content-Length".into());
    }
    let len = headers
        .get("content-length")
        .map(|v| v.parse::<usize>())
        .transpose()
        .map_err(|_| "Invalid Content-Length")?
        .unwrap_or(0);
    if len > MAX_BODY {
        return Err("Body exceeds 1 MiB".into());
    }
    let mut body = data[split + 4..].to_vec();
    if body.len() > len {
        return Err("Unexpected trailing bytes".into());
    }
    while body.len() < len {
        let wanted = chunk.len().min(len - body.len());
        let n = stream
            .read(&mut chunk[..wanted])
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("Incomplete HTTP body".into());
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Ok((first, headers, body))
}
pub fn read_request(stream: &mut TcpStream) -> Result<Request, String> {
    parse_request(&mut DeadlineReader::new(stream, Duration::from_secs(10)))
}
fn parse_request(stream: &mut impl Read) -> Result<Request, String> {
    let (first, headers, body) = read_message(stream)?;
    let p: Vec<_> = first.split_whitespace().collect();
    if p.len() != 3 || p[2] != "HTTP/1.1" || !p[1].starts_with('/') {
        return Err("Expected HTTP/1.1 origin-form request".into());
    }
    Ok(Request {
        method: p[0].into(),
        path: p[1].split('?').next().unwrap().into(),
        headers,
        body,
    })
}
pub fn write_response(stream: &mut TcpStream, r: Response) {
    let reason = match r.status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        _ => "Internal Server Error",
    };
    let header=format!("HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'; form-action 'self'\r\n\r\n",r.status,reason,r.content_type,r.body.len());
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(&r.body);
}
pub fn client_request(
    server: &str,
    path: &str,
    token: Option<&str>,
    body: &Value,
) -> Result<Value, String> {
    let address = server
        .strip_prefix("http://")
        .ok_or("Local version requires http:// on a trusted LAN")?
        .trim_end_matches('/');
    if address.contains('/')
        || address.contains('@')
        || address.contains('\r')
        || address.contains('\n')
    {
        return Err("Invalid server address".into());
    }
    let host = if address.contains(':') {
        address.to_string()
    } else {
        format!("{address}:80")
    };
    use std::net::ToSocketAddrs;
    let addresses = host.to_socket_addrs().map_err(|e| e.to_string())?;
    let mut last_error = "No reachable server address".to_string();
    let mut connected = None;
    for address in addresses.take(8) {
        match TcpStream::connect_timeout(&address, Duration::from_secs(5)) {
            Ok(stream) => {
                connected = Some(stream);
                break;
            }
            Err(e) => last_error = e.to_string(),
        }
    }
    let mut stream = connected.ok_or(last_error)?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    let data = serde_json::to_vec(body).map_err(|e| e.to_string())?;
    let auth = match token {
        Some(t) => {
            if t.contains('\r') || t.contains('\n') {
                return Err("Invalid token".into());
            }
            format!("Authorization: Bearer {t}\r\n")
        }
        None => String::new(),
    };
    let head=format!("POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{auth}Connection: close\r\n\r\n",data.len());
    stream
        .write_all(head.as_bytes())
        .and_then(|_| stream.write_all(&data))
        .map_err(|e| e.to_string())?;
    let (first, _, bytes) = read_message(&mut DeadlineReader::new(
        &mut stream,
        Duration::from_secs(10),
    ))?;
    let status = first.split_whitespace().nth(1).unwrap_or("500");
    let value: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if !status.starts_with('2') {
        return Err(format!(
            "HTTP {status}: {}",
            value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("request failed")
        ));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(raw: &[u8]) -> Result<Request, String> {
        parse_request(&mut std::io::Cursor::new(raw))
    }
    #[test]
    fn continuous_bytes_do_not_extend_the_read_deadline() {
        use std::net::TcpListener;
        use std::sync::mpsc;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = mpsc::channel();
        let sender = std::thread::spawn(move || {
            let mut socket = TcpStream::connect(address).unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            // Always finish within three seconds even if the deadline regresses.
            for _ in 0..300 {
                if socket.write_all(b"x").is_err()
                    || stopped.recv_timeout(Duration::from_millis(10)).is_ok()
                {
                    break;
                }
            }
        });
        let (mut socket, _) = listener.accept().unwrap();
        let mut received = Vec::new();
        let result =
            DeadlineReader::new(&mut socket, Duration::from_millis(100)).read_to_end(&mut received);
        let _ = stop.send(());
        sender.join().unwrap();
        let error = result.expect_err("Continuous traffic must still hit the absolute deadline");
        assert!(matches!(
            error.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ));
    }
    #[test]
    fn rejects_smuggling() {
        assert!(
            parse(b"POST / HTTP/1.1\r\nContent-Length: 0\r\nContent-Length: 1\r\n\r\n").is_err()
        );
        assert!(parse(b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n").is_err());
    }
    #[test]
    fn parses_body() {
        let r =
            parse(b"POST /api HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}").unwrap();
        assert_eq!(r.body, b"{}");
    }
    #[test]
    fn rejects_oversized_or_truncated() {
        assert!(parse(b"POST / HTTP/1.1\r\nContent-Length: 2000000\r\n\r\n").is_err());
        assert!(parse(b"POST / HTTP/1.1\r\nContent-Length: 3\r\n\r\n{}").is_err());
    }
}
