//! Upstream HTTP/1.1 client: plain TCP or verified Mbed TLS, chunked body
//! decoding, SSE line reads, and a small keep-alive pool per origin.

use std::collections::HashMap;
use std::ffi::{c_int, c_short, c_ulong};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::rules;
use crate::tls::TlsStream;

#[repr(C)]
struct PollFd {
    fd: c_int,
    events: c_short,
    revents: c_short,
}

unsafe extern "C" {
    fn poll(fds: *mut PollFd, nfds: c_ulong, timeout: c_int) -> c_int;
}

const POLLIN: c_short = 0x001;
const POLLERR: c_short = 0x008;
const POLLHUP: c_short = 0x010;
const POLLNVAL: c_short = 0x020;

fn fd_is_stale(fd: RawFd) -> bool {
    let mut pfd = PollFd {
        fd,
        events: POLLIN,
        revents: 0,
    };
    let rc = unsafe { poll(&mut pfd, 1, 0) };
    if rc < 0 {
        return true;
    }
    rc > 0 && (pfd.revents & (POLLIN | POLLERR | POLLHUP | POLLNVAL)) != 0
}

pub trait Transport: Read + Write + Send {
    fn set_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;
    fn is_stale(&self) -> bool;
}
impl Transport for TcpStream {
    fn set_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(timeout)?;
        self.set_write_timeout(timeout)
    }

    fn is_stale(&self) -> bool {
        fd_is_stale(self.as_raw_fd())
    }
}
impl Transport for TlsStream {
    fn set_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.set_socket_timeout(timeout)
    }

    fn is_stale(&self) -> bool {
        fd_is_stale(self.raw_fd())
    }
}

struct PooledConn {
    reader: BufReader<Box<dyn Transport>>,
    last_used: Instant,
}

struct Pool {
    idle: Mutex<HashMap<String, Vec<PooledConn>>>,
}

impl Pool {
    fn new() -> Self {
        Pool {
            idle: Mutex::new(HashMap::new()),
        }
    }

    fn take(&self, key: &str) -> Option<PooledConn> {
        let mut idle = self.idle.lock().ok()?;
        let list = idle.get_mut(key)?;
        while let Some(conn) = list.pop() {
            if conn.last_used.elapsed() < Duration::from_secs(30)
                && !conn.reader.get_ref().is_stale()
            {
                return Some(conn);
            }
        }
        None
    }

    fn put(&self, key: &str, conn: PooledConn) {
        if let Ok(mut idle) = self.idle.lock() {
            let list = idle.entry(key.to_string()).or_default();
            if list.len() < rules::MAX_IDLE_PER_HOST {
                list.push(conn);
            }
        }
    }
}

#[derive(Clone)]
pub struct HttpClient {
    pool: Arc<Pool>,
    timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct Url {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl Url {
    pub fn parse(raw: &str) -> Result<Url, String> {
        let (scheme, rest) = if let Some(rest) = raw.strip_prefix("https://") {
            ("https", rest)
        } else if let Some(rest) = raw.strip_prefix("http://") {
            ("http", rest)
        } else {
            return Err(format!("URL must start with http:// or https://: {raw}"));
        };
        let (authority, path) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, "/"),
        };
        let authority = authority.rsplit('@').next().unwrap_or(authority);
        if authority.is_empty() {
            return Err(format!("URL has no host: {raw}"));
        }
        let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
            let end = stripped
                .find(']')
                .ok_or_else(|| format!("invalid IPv6 host: {raw}"))?;
            let host = &stripped[..end];
            let tail = &stripped[end + 1..];
            let port = tail
                .strip_prefix(':')
                .and_then(|text| text.parse::<u16>().ok())
                .unwrap_or(if scheme == "https" { 443 } else { 80 });
            (host.to_string(), port)
        } else if let Some((host, port)) = authority.rsplit_once(':') {
            if port.chars().all(|ch| ch.is_ascii_digit()) {
                (
                    host.to_string(),
                    port.parse::<u16>()
                        .map_err(|_| format!("invalid port: {raw}"))?,
                )
            } else {
                (authority.to_string(), if scheme == "https" { 443 } else { 80 })
            }
        } else {
            (
                authority.to_string(),
                if scheme == "https" { 443 } else { 80 },
            )
        };
        if host.is_empty() {
            return Err(format!("URL has no host: {raw}"));
        }
        Ok(Url {
            scheme: scheme.to_string(),
            host,
            port,
            path: if path.is_empty() { "/" } else { path }.to_string(),
        })
    }

    pub fn key(&self) -> String {
        format!("{}://{}:{}", self.scheme, self.host, self.port)
    }

    pub fn host_header(&self) -> String {
        let default = if self.scheme == "https" { 443 } else { 80 };
        let host = if self.host.contains(':') && !self.host.starts_with('[') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == default {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    conn: Option<BufReader<Box<dyn Transport>>>,
    mode: BodyMode,
    reusable: bool,
    body_done: bool,
    line_buf: Vec<u8>,
    pool: Arc<Pool>,
    key: String,
    released: bool,
}

#[derive(Clone, Copy)]
enum BodyMode {
    Empty,
    Length(usize),
    Chunked { remaining: usize, done: bool },
    Eof,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn read_all(&mut self) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = self.read_body(&mut buf)?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
            if out.len() > rules::MAX_BODY_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "upstream response too large",
                ));
            }
        }
        self.release_if_done();
        Ok(out)
    }

    /// Read one line without the trailing newline. Returns 0 bytes at EOF.
    pub fn read_line(&mut self) -> io::Result<Vec<u8>> {
        loop {
            if let Some(pos) = self.line_buf.iter().position(|&b| b == b'\n') {
                let mut line: Vec<u8> = self.line_buf.drain(..=pos).collect();
                line.pop();
                if line.ends_with(b"\r") {
                    line.pop();
                }
                return Ok(line);
            }
            if self.line_buf.len() > rules::MAX_BODY_BYTES {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "SSE line too long"));
            }
            let mut buf = [0u8; 8 * 1024];
            let n = self.read_body(&mut buf)?;
            if n == 0 {
                let line = std::mem::take(&mut self.line_buf);
                return Ok(line);
            }
            self.line_buf.extend_from_slice(&buf[..n]);
        }
    }

    /// True once the response body has been fully consumed and no buffered
    /// line remains. `read_line` returns an empty vec at EOF forever; callers
    /// must check this before treating an empty line as an SSE separator.
    pub fn at_eof(&self) -> bool {
        self.body_done && self.line_buf.is_empty()
    }

    /// Decoded body bytes, without chunk framing. Used by streaming proxies.
    pub fn read_chunk(&mut self, out: &mut [u8]) -> io::Result<usize> {
        self.read_body(out)
    }

    fn read_body(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            match self.mode {
                BodyMode::Empty => {
                    self.body_done = true;
                    return Ok(0);
                }
                BodyMode::Length(remaining) => {
                    if remaining == 0 {
                        self.body_done = true;
                        return Ok(0);
                    }
                    let limit = out.len().min(remaining);
                    let n = self.read_conn(&mut out[..limit])?;
                    if n == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "upstream closed early",
                        ));
                    }
                    let remaining = remaining - n;
                    self.mode = BodyMode::Length(remaining);
                    if remaining == 0 {
                        self.body_done = true;
                    }
                    return Ok(n);
                }
                BodyMode::Chunked {
                    mut remaining,
                    mut done,
                } => {
                    if done {
                        self.body_done = true;
                        return Ok(0);
                    }
                    if remaining == 0 {
                        let line = self.read_transport_line()?;
                        let text = String::from_utf8_lossy(&line);
                        let size_text = text.split(';').next().unwrap_or("").trim();
                        let size = usize::from_str_radix(size_text, 16).map_err(|_| {
                            io::Error::new(io::ErrorKind::InvalidData, "invalid chunk size")
                        })?;
                        if size > rules::MAX_BODY_BYTES {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "upstream chunk too large",
                            ));
                        }
                        if size == 0 {
                            loop {
                                let trailer = self.read_transport_line()?;
                                if trailer.is_empty() {
                                    break;
                                }
                            }
                            done = true;
                            self.mode = BodyMode::Chunked { remaining: 0, done };
                            self.body_done = true;
                            return Ok(0);
                        }
                        remaining = size;
                    }
                    let limit = out.len().min(remaining);
                    let n = self.read_conn(&mut out[..limit])?;
                    if n == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "upstream closed mid-chunk",
                        ));
                    }
                    remaining -= n;
                    if remaining == 0 {
                        let mut crlf = [0u8; 2];
                        self.read_conn_exact(&mut crlf)?;
                    }
                    self.mode = BodyMode::Chunked { remaining, done };
                    return Ok(n);
                }
                BodyMode::Eof => {
                    let n = self.read_conn(out)?;
                    if n == 0 {
                        self.body_done = true;
                    }
                    return Ok(n);
                }
            }
        }
    }

    fn read_conn(&mut self, out: &mut [u8]) -> io::Result<usize> {
        self.conn
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "no connection"))?
            .read(out)
    }

    fn read_conn_exact(&mut self, out: &mut [u8]) -> io::Result<()> {
        self.conn
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "no connection"))?
            .read_exact(out)
    }

    fn read_transport_line(&mut self) -> io::Result<Vec<u8>> {
        let conn = self
            .conn
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "no connection"))?;
        read_bounded_line(conn, "upstream closed during chunk")
    }

    fn release_if_done(&mut self) {
        if self.released || !self.body_done || !self.reusable {
            return;
        }
        self.released = true;
        if let Some(reader) = self.conn.take() {
            self.pool.put(
                &self.key,
                PooledConn {
                    reader,
                    last_used: Instant::now(),
                },
            );
        }
    }
}

impl Drop for Response {
    fn drop(&mut self) {
        self.release_if_done();
    }
}

impl HttpClient {
    pub fn new(timeout: Duration) -> Self {
        HttpClient {
            pool: Arc::new(Pool::new()),
            timeout,
        }
    }

    pub fn request(
        &self,
        method: &str,
        url: &Url,
        headers: &[(String, String)],
        body: Option<&[u8]>,
        timeout: Option<Duration>,
    ) -> io::Result<Response> {
        let timeout = timeout.unwrap_or(self.timeout);
        let mut head = format!("{method} {} HTTP/1.1\r\nHost: {}\r\n", url.path, url.host_header());
        head.push_str("connection: keep-alive\r\naccept-encoding: identity\r\n");
        if !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("user-agent"))
        {
            head.push_str("user-agent: paig\r\n");
        }
        for (name, value) in headers {
            if name.contains('\r') || name.contains('\n') || value.contains('\r') || value.contains('\n') {
                continue;
            }
            let lower = name.to_ascii_lowercase();
            if matches!(
                lower.as_str(),
                "host" | "connection" | "content-length" | "transfer-encoding" | "accept-encoding"
            ) {
                continue;
            }
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
        if let Some(body) = body {
            head.push_str(&format!("content-length: {}\r\n", body.len()));
        }
        head.push_str("\r\n");

        let key = url.key();
        if let Some(conn) = self.pool.take(&key) {
            let _ = conn.reader.get_ref().set_timeout(Some(timeout));
            let result = self.exchange(conn.reader, head.as_bytes(), body, &key);
            // Only replay idempotent methods on a stale pooled socket. A POST
            // may already have reached the upstream; fetch-level retries own
            // that decision, so a single request cannot multiply POSTs here.
            if !matches!(method, "GET" | "HEAD") {
                return result;
            }
            match result {
                Ok(response) => return Ok(response),
                Err(_) => {
                    // Stale pooled socket: retry once on a fresh connection.
                }
            }
        }
        let conn = self.connect(url, timeout)?;
        self.exchange(conn, head.as_bytes(), body, &key)
    }

    fn connect(&self, url: &Url, timeout: Duration) -> io::Result<BufReader<Box<dyn Transport>>> {
        let transport: Box<dyn Transport> = if url.scheme == "https" {
            Box::new(TlsStream::connect(&url.host, url.port, timeout)?)
        } else {
            let mut last = None;
            let mut stream = None;
            for addr in (url.host.as_str(), url.port).to_socket_addrs()? {
                match TcpStream::connect_timeout(&addr, timeout) {
                    Ok(found) => {
                        stream = Some(found);
                        break;
                    }
                    Err(error) => last = Some(error),
                }
            }
            let stream = stream.ok_or_else(|| {
                last.unwrap_or_else(|| io::Error::new(io::ErrorKind::Other, "no address"))
            })?;
            stream.set_nodelay(true).ok();
            stream.set_read_timeout(Some(timeout))?;
            stream.set_write_timeout(Some(timeout))?;
            Box::new(stream)
        };
        Ok(BufReader::with_capacity(16 * 1024, transport))
    }

    fn exchange(
        &self,
        mut conn: BufReader<Box<dyn Transport>>,
        head: &[u8],
        body: Option<&[u8]>,
        key: &str,
    ) -> io::Result<Response> {
        conn.get_mut().write_all(head)?;
        if let Some(body) = body {
            conn.get_mut().write_all(body)?;
        }
        conn.get_mut().flush()?;

        let status_line = read_transport_line(&mut conn)?;
        let status_text = String::from_utf8_lossy(&status_line);
        let mut parts = status_text.split_whitespace();
        let _version = parts.next();
        let status = parts
            .next()
            .and_then(|text| text.parse::<u16>().ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid status line"))?;

        let mut headers = Vec::new();
        let mut total = status_line.len();
        loop {
            let line = read_transport_line(&mut conn)?;
            if line.is_empty() {
                break;
            }
            total += line.len();
            if total > rules::MAX_HEADER_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "upstream headers too large",
                ));
            }
            let text = String::from_utf8_lossy(&line);
            let Some((name, value)) = text.split_once(':') else { continue };
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }

        let connection = headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("connection"))
            .map(|(_, value)| value.as_str())
            .collect::<Vec<_>>();
        let close = connection.iter().any(|value| has_token(value, "close"));
        let keep_alive = !close;
        let chunked = headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("transfer-encoding"))
            .any(|(_, value)| has_token(value, "chunked"));
        let content_length = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok());

        let mode = if status == 204 || status == 304 {
            BodyMode::Empty
        } else if chunked {
            BodyMode::Chunked {
                remaining: 0,
                done: false,
            }
        } else if let Some(length) = content_length {
            BodyMode::Length(length)
        } else {
            BodyMode::Eof
        };
        let body_done = matches!(mode, BodyMode::Empty | BodyMode::Length(0));
        let reusable = keep_alive && !matches!(mode, BodyMode::Eof);

        Ok(Response {
            status,
            headers,
            conn: Some(conn),
            mode,
            reusable,
            body_done,
            line_buf: Vec::new(),
            pool: Arc::clone(&self.pool),
            key: key.to_string(),
            released: false,
        })
    }
}

fn read_transport_line(conn: &mut BufReader<Box<dyn Transport>>) -> io::Result<Vec<u8>> {
    read_bounded_line(conn, "upstream closed early")
}

fn read_bounded_line(
    conn: &mut BufReader<Box<dyn Transport>>,
    eof_message: &str,
) -> io::Result<Vec<u8>> {
    let mut line = Vec::new();
    loop {
        let available = conn.fill_buf()?;
        if available.is_empty() {
            if line.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    eof_message.to_string(),
                ));
            }
            break;
        }
        if let Some(position) = available.iter().position(|&byte| byte == b'\n') {
            let take = position + 1;
            if line.len().saturating_add(take) > rules::MAX_HEADER_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "upstream line too long",
                ));
            }
            line.extend_from_slice(&available[..take]);
            conn.consume(take);
            break;
        }
        if line.len().saturating_add(available.len()) > rules::MAX_HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "upstream line too long",
            ));
        }
        line.extend_from_slice(available);
        let take = available.len();
        conn.consume(take);
    }
    while line.last() == Some(&b'\n') || line.last() == Some(&b'\r') {
        line.pop();
    }
    Ok(line)
}

fn has_token(value: &str, token: &str) -> bool {
    value
        .split(',')
        .any(|part| part.trim().eq_ignore_ascii_case(token))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::os::unix::net::UnixStream;
    use std::thread;

    #[test]
    fn url_parsing_defaults_and_ports() {
        let a = Url::parse("https://Example.com/v1/chat?x=1").unwrap();
        assert_eq!(a.scheme, "https");
        assert_eq!(a.host, "Example.com");
        assert_eq!(a.port, 443);
        assert_eq!(a.path, "/v1/chat?x=1");
        let b = Url::parse("http://127.0.0.1:1234").unwrap();
        assert_eq!(b.port, 1234);
        assert_eq!(b.path, "/");
        let v6 = Url::parse("https://[::1]:8443/x").unwrap();
        assert_eq!(v6.host, "::1");
        assert_eq!(v6.host_header(), "[::1]:8443");
        assert!(Url::parse("ftp://x").is_err());
    }

    #[test]
    fn http_get_with_content_length() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nhi")
                .unwrap();
        });
        let client = HttpClient::new(Duration::from_secs(3));
        let url = Url::parse(&format!("http://127.0.0.1:{port}/x")).unwrap();
        let mut response = client.request("GET", &url, &[], None, None).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.read_all().unwrap(), b"hi");
    }

    #[test]
    fn http_chunked_response_is_decoded() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
                )
                .unwrap();
        });
        let client = HttpClient::new(Duration::from_secs(3));
        let url = Url::parse(&format!("http://127.0.0.1:{port}/x")).unwrap();
        let mut response = client.request("GET", &url, &[], None, None).unwrap();
        assert_eq!(response.read_all().unwrap(), b"hello");
    }

    #[test]
    fn stale_probe_detects_closed_peer() {
        let (live, dead) = UnixStream::pair().unwrap();
        assert!(!fd_is_stale(live.as_raw_fd()));
        drop(dead);
        assert!(fd_is_stale(live.as_raw_fd()));
    }

    #[test]
    fn sse_lines_are_read_from_body() {
        let mut body = crate::sse::Parser::new();
        let events = body.push(b"data: {\"a\":1}\n\ndata: [DONE]\n\n");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "{\"a\":1}");
        assert_eq!(events[1].data, "[DONE]");
    }

    #[test]
    fn caller_user_agent_replaces_the_default() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (sender, receiver) = std::sync::mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap();
            sender
                .send(String::from_utf8_lossy(&buf[..n]).to_string())
                .unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .unwrap();
        });
        let client = HttpClient::new(Duration::from_secs(3));
        let url = Url::parse(&format!("http://127.0.0.1:{port}/x")).unwrap();
        let mut response = client
            .request(
                "GET",
                &url,
                &[("user-agent".to_string(), "cli".to_string())],
                None,
                None,
            )
            .unwrap();
        let _ = response.read_all().unwrap();
        handle.join().unwrap();
        let request = receiver.recv().unwrap();
        assert_eq!(request.to_ascii_lowercase().matches("user-agent:").count(), 1);
        assert!(request.contains("user-agent: cli"));
    }
}
