//! Minimal HTTP/1.1 client for the Docker Engine API over its Unix socket.
//! Structured JSON only: nothing here runs the `docker` CLI or parses terminal output.
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// Largest non-streaming response accepted (an `inspect` or `df` is far below this).
const MAX_BODY: u64 = 64 << 20;
const MAX_HEAD: usize = 64 << 10;

#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// Socket missing or connection refused: the daemon is not running (or not installed).
    NotRunning(String),
    /// EACCES on the socket: this user is not allowed to manage Docker.
    Denied(String),
    Timeout,
    /// The engine answered with an error status; `message` is the engine's own text.
    Api { status: u16, message: String },
    Protocol(String),
    /// Rejected before anything was sent (validation).
    Invalid(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotRunning(s) => write!(f, "Docker daemon not reachable: {s}"),
            Error::Denied(s) => write!(f, "permission denied on the Docker socket ({s}); only root and members of the 'docker' group may manage Docker"),
            Error::Timeout => write!(f, "the Docker daemon did not answer in time"),
            Error::Api { status, message } => write!(f, "{message} (HTTP {status})"),
            Error::Protocol(s) => write!(f, "unexpected response from Docker: {s}"),
            Error::Invalid(s) => write!(f, "{s}"),
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        match e.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => Error::Timeout,
            _ => Error::Protocol(e.to_string()),
        }
    }
}

pub struct Resp {
    pub status: u16,
    pub body: Vec<u8>,
}

pub fn connect(sock: &Path, timeout: Duration) -> Result<UnixStream, Error> {
    let s = UnixStream::connect(sock).map_err(|e| match e.kind() {
        io::ErrorKind::PermissionDenied => Error::Denied(sock.display().to_string()),
        _ => Error::NotRunning(format!("{}: {e}", sock.display())),
    })?;
    s.set_read_timeout(Some(timeout))?;
    s.set_write_timeout(Some(timeout))?;
    Ok(s)
}

fn write_request(s: &mut UnixStream, method: &str, path: &str, body: Option<&[u8]>, upgrade: bool) -> io::Result<()> {
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: docker\r\nUser-Agent: SystemMonitor/{}\r\n", env!("CARGO_PKG_VERSION"));
    head.push_str(if upgrade { "Connection: Upgrade\r\nUpgrade: tcp\r\n" } else { "Connection: close\r\n" });
    if let Some(b) = body {
        head.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", b.len()));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes())?;
    if let Some(b) = body {
        s.write_all(b)?;
    }
    s.flush()
}

/// Status code and lower-cased headers.
fn read_head(r: &mut impl BufRead) -> Result<(u16, Vec<(String, String)>), Error> {
    let mut line = String::new();
    let mut total = 0;
    if r.read_line(&mut line)? == 0 {
        return Err(Error::Protocol("connection closed before a response".into()));
    }
    let status = line.split_whitespace().nth(1).and_then(|s| s.parse::<u16>().ok()).ok_or_else(|| Error::Protocol(format!("bad status line {:?}", line.trim())))?;
    let mut headers = vec![];
    loop {
        line.clear();
        let n = r.read_line(&mut line)?;
        total += n;
        if n == 0 || line.trim().is_empty() {
            break;
        }
        if total > MAX_HEAD {
            return Err(Error::Protocol("response headers too large".into()));
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    Ok((status, headers))
}

fn header<'a>(h: &'a [(String, String)], k: &str) -> Option<&'a str> {
    h.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str())
}

/// Body reader honouring chunked encoding / Content-Length / close-delimited bodies.
fn body_reader<R: BufRead + Send + 'static>(r: R, headers: &[(String, String)]) -> Box<dyn BufRead + Send> {
    if header(headers, "transfer-encoding").map(|v| v.to_ascii_lowercase().contains("chunked")).unwrap_or(false) {
        Box::new(BufReader::new(Chunked::new(r)))
    } else if let Some(n) = header(headers, "content-length").and_then(|v| v.parse::<u64>().ok()) {
        Box::new(r.take(n))
    } else {
        Box::new(r)
    }
}

/// Engine error text from `{"message": "..."}`, falling back to the raw body.
pub fn error_message(body: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(String::from))
        .unwrap_or_else(|| String::from_utf8_lossy(body).trim().chars().take(300).collect())
}

pub fn request(sock: &Path, timeout: Duration, method: &str, path: &str, body: Option<&[u8]>) -> Result<Resp, Error> {
    let mut s = connect(sock, timeout)?;
    write_request(&mut s, method, path, body, false)?;
    let mut r = BufReader::new(s);
    let (status, headers) = read_head(&mut r)?;
    let mut out = vec![];
    body_reader(r, &headers).take(MAX_BODY + 1).read_to_end(&mut out)?;
    if out.len() as u64 > MAX_BODY {
        return Err(Error::Protocol("response too large".into()));
    }
    Ok(Resp { status, body: out })
}

/// Unblocks a stream that another thread is reading (shuts the socket down).
pub struct Canceller(UnixStream);

impl Canceller {
    pub fn cancel(&self) {
        let _ = self.0.shutdown(std::net::Shutdown::Both);
    }
}

/// A long-lived response (events, logs, pull progress). Reads block until data arrives or `Canceller::cancel`.
pub struct Stream {
    pub reader: Box<dyn BufRead + Send>,
    cancel: UnixStream,
}

impl Stream {
    pub fn canceller(&self) -> Result<Canceller, Error> {
        Ok(Canceller(self.cancel.try_clone()?))
    }
}

pub fn stream(sock: &Path, connect_timeout: Duration, method: &str, path: &str, body: Option<&[u8]>) -> Result<Stream, Error> {
    let mut s = connect(sock, connect_timeout)?;
    write_request(&mut s, method, path, body, false)?;
    let cancel = s.try_clone()?;
    let mut r = BufReader::new(s);
    let (status, headers) = read_head(&mut r)?;
    if status >= 400 {
        let mut b = vec![];
        let _ = body_reader(r, &headers).take(1 << 16).read_to_end(&mut b);
        return Err(Error::Api { status, message: error_message(&b) });
    }
    // Headers arrived; from here the stream may legitimately stay silent for a long time.
    cancel.set_read_timeout(None)?;
    Ok(Stream { reader: body_reader(r, &headers), cancel })
}

/// Hijacked bidirectional connection (exec). The reader keeps any bytes buffered after the 101 response.
pub fn upgrade(sock: &Path, timeout: Duration, path: &str, body: &[u8]) -> Result<(BufReader<UnixStream>, UnixStream), Error> {
    let mut s = connect(sock, timeout)?;
    write_request(&mut s, "POST", path, Some(body), true)?;
    let writer = s.try_clone()?;
    let mut r = BufReader::new(s);
    let (status, headers) = read_head(&mut r)?;
    if status != 101 && status != 200 {
        let mut b = vec![];
        let _ = body_reader(r, &headers).take(1 << 16).read_to_end(&mut b);
        return Err(Error::Api { status, message: error_message(&b) });
    }
    writer.set_read_timeout(None)?;
    Ok((r, writer))
}

/// `Transfer-Encoding: chunked` decoder.
pub struct Chunked<R> {
    inner: R,
    left: u64,
    done: bool,
}

impl<R: BufRead> Chunked<R> {
    pub fn new(inner: R) -> Self {
        Self { inner, left: 0, done: false }
    }
}

impl<R: BufRead> Read for Chunked<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        if self.left == 0 {
            let mut line = String::new();
            if self.inner.read_line(&mut line)? == 0 {
                self.done = true;
                return Ok(0);
            }
            let hex = line.trim().split(';').next().unwrap_or("");
            self.left = u64::from_str_radix(hex, 16).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("bad chunk size {hex:?}")))?;
            if self.left == 0 {
                self.done = true;
                loop {
                    line.clear();
                    if self.inner.read_line(&mut line)? == 0 || line.trim().is_empty() {
                        break;
                    }
                }
                return Ok(0);
            }
        }
        let want = buf.len().min(self.left as usize);
        let n = self.inner.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.left -= n as u64;
        if self.left == 0 {
            let mut crlf = [0u8; 2];
            self.inner.read_exact(&mut crlf)?;
        }
        Ok(n)
    }
}

/// One frame of Docker's multiplexed stdout/stderr stream (non-TTY logs and exec).
/// Returns `(stream, payload)`; stream 1 = stdout, 2 = stderr.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut h = [0u8; 8];
    match r.read_exact(&mut h) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = u32::from_be_bytes([h[4], h[5], h[6], h[7]]) as usize;
    if h[0] > 3 || h[1..4] != [0, 0, 0] || n > 8 << 20 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a multiplexed stream"));
    }
    let mut p = vec![0u8; n];
    r.read_exact(&mut p)?;
    Ok(Some((h[0], p)))
}

/// Percent-encode a path segment or query value (RFC 3986 unreserved characters stay as they are).
pub fn enc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}
