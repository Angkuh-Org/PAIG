//! Minimal HTTP/1.1 server: request parsing, bounded bodies, keep-alive, JSON
//! responses, and chunked SSE. No async runtime; a small fixed worker pool.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::json::Value;
use crate::rules;
use crate::util::{loge, logw};

#[derive(Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub version: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header_all(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
            .collect()
    }

    pub fn json(&self) -> Result<Value, String> {
        Value::parse(&self.body).map_err(|error| error.to_string())
    }

    pub fn keep_alive(&self) -> bool {
        let connection = self.header_all("connection");
        if connection.iter().any(|value| has_token(value, "close")) {
            return false;
        }
        if self.version.eq_ignore_ascii_case("HTTP/1.0") {
            return connection.iter().any(|value| has_token(value, "keep-alive"));
        }
        true
    }
}

fn has_token(value: &str, token: &str) -> bool {
    value
        .split(',')
        .any(|part| part.trim_ascii().eq_ignore_ascii_case(token))
}

#[derive(Debug)]
pub struct HttpError {
    pub status: u16,
    pub message: String,
}

impl HttpError {
    pub fn new(status: u16, message: &str) -> Self {
        HttpError {
            status,
            message: message.to_string(),
        }
    }
}

impl From<io::Error> for HttpError {
    fn from(error: io::Error) -> Self {
        HttpError::new(400, &format!("cannot read request: {error}"))
    }
}

pub struct Response {
    stream: TcpStream,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub keep_alive: bool,
    pub headers_sent: bool,
    chunked: bool,
    finished: bool,
}

impl Response {
    pub fn new(stream: TcpStream, keep_alive: bool) -> Self {
        Response {
            stream,
            status: 0,
            headers: Vec::new(),
            keep_alive,
            headers_sent: false,
            chunked: false,
            finished: false,
        }
    }

    pub fn set_header(&mut self, name: &str, value: &str) {
        if let Some((_, slot)) = self
            .headers
            .iter_mut()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
        {
            *slot = value.to_string();
        } else {
            self.headers.push((name.to_string(), value.to_string()));
        }
    }

    pub fn send_json(&mut self, status: u16, value: &Value) -> io::Result<()> {
        let body = value.stringify();
        self.send_bytes(status, "application/json; charset=utf-8", body.as_bytes())
    }

    pub fn send_error(&mut self, status: u16, message: &str) -> io::Result<()> {
        let mut error = Value::obj();
        error.insert("message", Value::string(message));
        error.insert("type", Value::string("bridge_error"));
        let mut root = Value::obj();
        root.insert("error", error);
        self.send_json(status, &root)
    }

    pub fn send_openai_error(
        &mut self,
        status: u16,
        message: &str,
        kind: &str,
        code: Option<&str>,
    ) -> io::Result<()> {
        let mut error = Value::obj();
        error.insert("message", Value::string(message));
        error.insert("type", Value::string(kind));
        if let Some(code) = code {
            error.insert("code", Value::string(code));
        }
        let mut root = Value::obj();
        root.insert("error", error);
        self.send_json(status, &root)
    }

    pub fn send_bytes(&mut self, status: u16, content_type: &str, body: &[u8]) -> io::Result<()> {
        if self.headers_sent {
            return Ok(());
        }
        self.status = status;
        self.set_header("content-type", content_type);
        self.set_header("content-length", &body.len().to_string());
        self.write_headers()?;
        self.stream.write_all(body)?;
        self.stream.flush()?;
        self.finished = true;
        Ok(())
    }

    pub fn start_sse(&mut self) -> io::Result<()> {
        if self.headers_sent {
            return Ok(());
        }
        self.status = 200;
        self.set_header("content-type", "text/event-stream; charset=utf-8");
        self.set_header("cache-control", "no-cache, no-transform");
        self.set_header("x-accel-buffering", "no");
        self.set_header("transfer-encoding", "chunked");
        self.chunked = true;
        self.write_headers()
    }

    pub fn sse_write(&mut self, data: &[u8]) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        if !self.headers_sent {
            self.start_sse()?;
        }
        if self.chunked {
            let head = format!("{:x}\r\n", data.len());
            self.stream.write_all(head.as_bytes())?;
            self.stream.write_all(data)?;
            self.stream.write_all(b"\r\n")?;
        } else {
            self.stream.write_all(data)?;
        }
        if data.len() >= rules::STREAM_FLUSH_BYTES {
            self.stream.flush()?;
        }
        Ok(())
    }

    pub fn sse_end(&mut self) -> io::Result<()> {
        if !self.headers_sent {
            self.start_sse()?;
        }
        if self.chunked {
            self.stream.write_all(b"0\r\n\r\n")?;
        }
        self.stream.flush()?;
        self.finished = true;
        Ok(())
    }

    pub fn send_empty(&mut self, status: u16) -> io::Result<()> {
        if self.headers_sent {
            return Ok(());
        }
        self.status = status;
        self.write_headers()?;
        self.stream.flush()?;
        self.finished = true;
        Ok(())
    }

    /// Best-effort probe used by non-streaming handlers to stop waiting on an
    /// upstream after the client has gone away.
    pub fn client_closed(&self) -> bool {
        if self.stream.set_nonblocking(true).is_err() {
            return false;
        }
        let mut byte = [0u8; 1];
        let result = self.stream.peek(&mut byte);
        let _ = self.stream.set_nonblocking(false);
        match result {
            Ok(0) => true,
            Err(error) => matches!(
                error.kind(),
                io::ErrorKind::ConnectionReset
                    | io::ErrorKind::BrokenPipe
                    | io::ErrorKind::NotConnected
            ),
            _ => false,
        }
    }

    pub fn finish(&mut self) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        if !self.headers_sent {
            return self.send_empty(204);
        }
        self.stream.flush()?;
        self.finished = true;
        Ok(())
    }

    fn write_headers(&mut self) -> io::Result<()> {
        if self.headers_sent {
            return Ok(());
        }
        self.headers_sent = true;
        let reason = reason_phrase(self.status);
        let mut head = format!("HTTP/1.1 {} {}\r\n", self.status, reason);
        for (name, value) in &self.headers {
            if value.contains('\r') || value.contains('\n') {
                continue;
            }
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
        head.push_str(if self.keep_alive {
            "connection: keep-alive\r\n\r\n"
        } else {
            "connection: close\r\n\r\n"
        });
        self.stream.write_all(head.as_bytes())
    }
}

pub fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Unknown",
    }
}

fn read_more(stream: &mut TcpStream, buf: &mut Vec<u8>) -> io::Result<usize> {
    const CHUNK: usize = 16 * 1024;
    let len = buf.len();
    buf.reserve(CHUNK);
    let spare = buf.spare_capacity_mut();
    // SAFETY: reserve guarantees CHUNK writable bytes after `len`; read()
    // initializes the prefix it reports, and only that prefix is committed.
    let dst = unsafe { std::slice::from_raw_parts_mut(spare.as_mut_ptr() as *mut u8, CHUNK) };
    let n = stream.read(dst)?;
    unsafe { buf.set_len(len + n) };
    Ok(n)
}

fn find_header_end(buf: &[u8], from: usize) -> Option<usize> {
    let window = &buf[from..];
    if let Some(pos) = window.windows(4).position(|window| window == b"\r\n\r\n") {
        return Some(from + pos + 4);
    }
    if let Some(pos) = window.windows(2).position(|window| window == b"\n\n") {
        return Some(from + pos + 2);
    }
    None
}

pub fn read_request(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Result<Option<Request>, HttpError> {
    let mut scanned = 0usize;
    let header_end = loop {
        if let Some(end) = find_header_end(buf, scanned) {
            break end;
        }
        if buf.len() > rules::MAX_HEADER_BYTES {
            return Err(HttpError::new(431, "request headers too large"));
        }
        // A terminator can only start in the last three bytes we already saw;
        // continuing from there makes header assembly linear instead of n^2.
        scanned = buf.len().saturating_sub(3);
        match read_more(stream, buf) {
            Ok(0) => {
                if buf.is_empty() {
                    return Ok(None);
                }
                return Err(HttpError::new(400, "connection closed during headers"));
            }
            Ok(_) => continue,
            Err(error) => {
                if error.kind() == io::ErrorKind::WouldBlock || error.kind() == io::ErrorKind::TimedOut {
                    return Err(HttpError::new(408, "request timeout"));
                }
                return Err(HttpError::from(error));
            }
        }
    };

    if header_end > rules::MAX_HEADER_BYTES {
        return Err(HttpError::new(431, "request headers too large"));
    }
    let (method, path, version, headers, content_length, chunked) = {
        let text = match std::str::from_utf8(&buf[..header_end]) {
            Ok(text) => std::borrow::Cow::Borrowed(text),
            Err(_) => String::from_utf8_lossy(&buf[..header_end]),
        };
        let mut lines = text.split('\n');
        let request_line = lines.next().unwrap_or("").trim_end_matches('\r');
        let mut parts = request_line.split_ascii_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let target = parts.next().unwrap_or("").to_string();
        let version = parts.next().unwrap_or("HTTP/1.1").to_string();
        if method.is_empty() || target.is_empty() {
            return Err(HttpError::new(400, "malformed request line"));
        }
        let path = target
            .strip_prefix("http://")
            .or_else(|| target.strip_prefix("https://"))
            .and_then(|rest| rest.find('/').map(|index| &rest[index..]))
            .unwrap_or(target.as_str())
            .to_string();

        let mut headers = Vec::with_capacity(16);
        for line in lines {
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                break;
            }
            let Some((name, value)) = line.split_once(':') else { continue };
            let name = name.trim_ascii();
            if name.is_empty() {
                continue;
            }
            headers.push((name.to_string(), value.trim_ascii().to_string()));
        }

        let mut content_length = None;
        for (name, value) in &headers {
            if !name.eq_ignore_ascii_case("content-length") {
                continue;
            }
            let parsed = value
                .trim_ascii()
                .parse::<usize>()
                .map_err(|_| HttpError::new(400, "invalid content-length"))?;
            if let Some(existing) = content_length {
                if existing != parsed {
                    return Err(HttpError::new(400, "conflicting content-length"));
                }
            } else {
                content_length = Some(parsed);
            }
        }
        let chunked = headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("transfer-encoding"))
            .any(|(_, value)| has_token(value, "chunked"));
        (method, path, version, headers, content_length, chunked)
    };
    buf.drain(..header_end);

    if headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("expect") && has_token(value, "100-continue")
    }) && (chunked || content_length.unwrap_or(0) > 0)
    {
        stream
            .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
            .map_err(HttpError::from)?;
        stream.flush().map_err(HttpError::from)?;
    }

    let body = if chunked {
        read_chunked_body(stream, buf)?
    } else if let Some(length) = content_length {
        if length > rules::MAX_BODY_BYTES {
            return Err(HttpError::new(413, "request body too large"));
        }
        read_exact_body(stream, buf, length)?
    } else {
        Vec::new()
    };

    Ok(Some(Request {
        method,
        path,
        version,
        headers,
        body,
    }))
}

fn read_exact_body(stream: &mut TcpStream, buf: &mut Vec<u8>, length: usize) -> Result<Vec<u8>, HttpError> {
    if buf.len() < length {
        // content-length is a claim from the peer: commit at most a small
        // prefix up front and let read_more grow as real bytes arrive.
        buf.reserve((length - buf.len()).min(256 * 1024));
    }
    while buf.len() < length {
        match read_more(stream, buf) {
            Ok(0) => return Err(HttpError::new(400, "connection closed during body")),
            Ok(_) => {}
            Err(error) => return Err(HttpError::from(error)),
        }
    }
    // Hand the body allocation to the caller and keep any pipelined bytes as
    // the next request's buffer; split_off copies only that small tail.
    let tail = buf.split_off(length);
    Ok(std::mem::replace(buf, tail))
}

fn read_line(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Result<Vec<u8>, HttpError> {
    loop {
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = buf.drain(..=pos).collect();
            if line.ends_with(b"\n") {
                line.pop();
            }
            if line.ends_with(b"\r") {
                line.pop();
            }
            return Ok(line);
        }
        if buf.len() > rules::MAX_HEADER_BYTES {
            return Err(HttpError::new(400, "line too long"));
        }
        match read_more(stream, buf) {
            Ok(0) => return Err(HttpError::new(400, "connection closed during chunk")),
            Ok(_) => {}
            Err(error) => return Err(HttpError::from(error)),
        }
    }
}

fn read_chunked_body(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Result<Vec<u8>, HttpError> {
    let mut body = Vec::new();
    loop {
        let line = read_line(stream, buf)?;
        let text = String::from_utf8_lossy(&line);
        let size_text = text.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| HttpError::new(400, "invalid chunk size"))?;
        if size == 0 {
            // Consume trailers.
            loop {
                let trailer = read_line(stream, buf)?;
                if trailer.is_empty() {
                    break;
                }
            }
            break;
        }
        if size > rules::MAX_BODY_BYTES
            || body.len() > rules::MAX_BODY_BYTES.saturating_sub(size)
        {
            return Err(HttpError::new(413, "request body too large"));
        }
        let framing = size
            .checked_add(2)
            .ok_or_else(|| HttpError::new(413, "request body too large"))?;
        while buf.len() < framing {
            match read_more(stream, buf) {
                Ok(0) => return Err(HttpError::new(400, "connection closed during chunk")),
                Ok(_) => {}
                Err(error) => return Err(HttpError::from(error)),
            }
        }
        body.extend_from_slice(&buf[..size]);
        buf.drain(..framing);
    }
    Ok(body)
}

pub fn serve<F>(listener: TcpListener, handler: Arc<F>) -> io::Result<()>
where
    F: Fn(&mut Request, &mut Response) + Send + Sync + 'static,
{
    let (sender, receiver) = mpsc::channel::<TcpStream>();
    let receiver = Arc::new(Mutex::new(receiver));
    let active = Arc::new(AtomicUsize::new(0));
    let pending = Arc::new(AtomicUsize::new(0));

    for _ in 0..rules::WORKER_THREADS {
        let receiver = Arc::clone(&receiver);
        let handler = Arc::clone(&handler);
        let active = Arc::clone(&active);
        let pending = Arc::clone(&pending);
        thread::Builder::new()
            .name("paig-worker".to_string())
            .stack_size(rules::THREAD_STACK_BYTES)
            .spawn(move || loop {
                let stream = {
                    let guard = match receiver.lock() {
                        Ok(guard) => guard,
                        Err(_) => break,
                    };
                    match guard.recv() {
                        Ok(stream) => {
                            pending.fetch_sub(1, Ordering::Relaxed);
                            stream
                        }
                        Err(_) => break,
                    }
                };
                let _guard = ActiveGuard::new(Arc::clone(&active));
                if let Err(error) = handle_connection(stream, &*handler) {
                    if error.kind() != io::ErrorKind::BrokenPipe
                        && error.kind() != io::ErrorKind::ConnectionReset
                    {
                        logw!("connection error: {error}");
                    }
                }
            })?;
    }

    let mut counter = 0usize;
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                loge!("accept failed: {error}");
                continue;
            }
        };
        counter = counter.wrapping_add(1);
        if active.load(Ordering::Relaxed) + pending.load(Ordering::Relaxed)
            >= rules::MAX_CONNECTIONS
        {
            let mut response = Response::new(stream, false);
            let _ = response.send_error(503, "too many connections");
            continue;
        }
        pending.fetch_add(1, Ordering::Relaxed);
        if sender.send(stream).is_err() {
            pending.fetch_sub(1, Ordering::Relaxed);
            break;
        }
        if counter % 1024 == 0 {
            logw!("accepted {counter} connections");
        }
    }
    Ok(())
}

struct ActiveGuard {
    active: Arc<AtomicUsize>,
}

impl ActiveGuard {
    fn new(active: Arc<AtomicUsize>) -> Self {
        active.fetch_add(1, Ordering::Relaxed);
        ActiveGuard { active }
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::Relaxed);
    }
}

fn handle_connection<F>(mut stream: TcpStream, handler: &F) -> io::Result<()>
where
    F: Fn(&mut Request, &mut Response),
{
    stream.set_nodelay(true).ok();
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    stream.set_write_timeout(Some(Duration::from_secs(120)))?;
    let mut buf = Vec::with_capacity(8 * 1024);
    loop {
        let mut request = match read_request(&mut stream, &mut buf) {
            Ok(Some(request)) => request,
            Ok(None) => return Ok(()),
            Err(error) => {
                if let Ok(write_stream) = stream.try_clone() {
                    let mut response = Response::new(write_stream, false);
                    let _ = response.send_error(error.status, &error.message);
                }
                return Ok(());
            }
        };
        let keep_alive = request.keep_alive();
        let write_stream = stream.try_clone()?;
        let mut response = Response::new(write_stream, keep_alive);
        handler(&mut request, &mut response);
        response.finish()?;
        if !keep_alive {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};

    #[test]
    fn detects_a_closed_client() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        let response = Response::new(server, true);
        assert!(!response.client_closed());
        drop(client);
        std::thread::sleep(Duration::from_millis(50));
        assert!(response.client_closed());
    }

    fn loopback_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        server.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        (client, server)
    }

    #[test]
    fn read_request_survives_split_headers_and_keeps_the_tail() {
        let (mut client, mut server) = loopback_pair();
        client
            .write_all(b"POST /v1/chat HTTP/1.1\r\nHost: x\r\ncontent-len")
            .unwrap();
        client.flush().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        client
            .write_all(b"gth: 5\r\n\r\nhelloGET /health HTTP/1.1\r\n\r\n")
            .unwrap();
        client.flush().unwrap();

        let mut buf = Vec::new();
        let request = read_request(&mut server, &mut buf).unwrap().unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/v1/chat");
        assert_eq!(request.body.as_slice(), b"hello".as_slice());
        assert_eq!(buf.as_slice(), b"GET /health HTTP/1.1\r\n\r\n".as_slice());

        let request = read_request(&mut server, &mut buf).unwrap().unwrap();
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/health");
        assert!(buf.is_empty());
    }

    #[test]
    fn read_request_decodes_a_chunked_body() {
        let (mut client, mut server) = loopback_pair();
        client
            .write_all(
                b"POST /v1/chat HTTP/1.1\r\nHost: x\r\ntransfer-encoding: chunked\r\n\r\n\
                  5\r\nhello\r\n0\r\n\r\n",
            )
            .unwrap();
        client.flush().unwrap();

        let mut buf = Vec::new();
        let request = read_request(&mut server, &mut buf).unwrap().unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.body.as_slice(), b"hello".as_slice());
        assert!(buf.is_empty());
    }
}
