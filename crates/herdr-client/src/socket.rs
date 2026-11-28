//! Minimal typed transport for the herdr local socket API.
//!
//! Newline-delimited JSON over a Unix domain socket, or a Windows named
//! pipe on win32 (`\\.\pipe\<socket path>`). One request per connection;
//! the first response line with a matching `id` resolves the request.
//! `events.subscribe` stays open and streams `{event, data}` envelopes.
//! Mirrors `packages/herdr-client/src/socket.ts` request/subscribe.

use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct HerdrRpcError {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for HerdrRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for HerdrRpcError {}

impl HerdrRpcError {
    pub fn from_value(value: &Value) -> Self {
        HerdrRpcError {
            code: value
                .get("code")
                .and_then(|v| v.as_str())
                .unwrap_or("error")
                .to_string(),
            message: value
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        }
    }
}

#[derive(Debug)]
pub enum ClientError {
    Rpc(HerdrRpcError),
    Io(String),
    Timeout(String),
    Closed(String),
    Malformed(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Rpc(e) => write!(f, "{}", e),
            ClientError::Io(m) => write!(f, "io: {}", m),
            ClientError::Timeout(m) => write!(f, "{}", m),
            ClientError::Closed(m) => write!(f, "{}", m),
            ClientError::Malformed(m) => write!(f, "malformed: {}", m),
        }
    }
}
impl std::error::Error for ClientError {}

impl From<std::io::Error> for ClientError {
    fn from(e: std::io::Error) -> Self {
        ClientError::Io(e.to_string())
    }
}

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

fn next_id(prefix: &str) -> String {
    let n = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    format!("{}-{}-{}", prefix, std::process::id(), n)
}

/// Map a logical herdr socket path to the transport endpoint used for
/// connect: on win32 the herdr socket is a Windows named pipe
/// (`\\.\pipe\<path>`), elsewhere it is the filesystem path itself.
pub fn socket_endpoint(socket_path: &str) -> String {
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\{}", socket_path)
    }
    #[cfg(not(windows))]
    {
        socket_path.to_string()
    }
}

/// A connected socket. Unix sockets and Windows named pipes (opened as
/// files) both support `try_clone` for independent read/write halves.
enum Conn {
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
    #[cfg(windows)]
    Pipe(std::fs::File),
}

impl Conn {
    fn connect(socket_path: &str) -> Result<Self, ClientError> {
        let endpoint = socket_endpoint(socket_path);
        #[cfg(unix)]
        {
            Ok(Conn::Unix(std::os::unix::net::UnixStream::connect(
                &endpoint,
            )?))
        }
        #[cfg(windows)]
        {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&endpoint)
                .map_err(|e| ClientError::Io(format!("connect {}: {}", endpoint, e)))?;
            Ok(Conn::Pipe(file))
        }
    }

    fn set_read_timeout(&self, dur: Option<Duration>) -> Result<(), ClientError> {
        match self {
            #[cfg(unix)]
            Conn::Unix(s) => Ok(s.set_read_timeout(dur)?),
            #[cfg(windows)]
            Conn::Pipe(_) => Ok(()), // no portable pipe read timeout
        }
    }

    fn write_line(&mut self, line: &str) -> Result<(), ClientError> {
        match self {
            #[cfg(unix)]
            Conn::Unix(s) => {
                s.write_all(line.as_bytes())?;
                s.write_all(b"\n")?;
                s.flush()?;
            }
            #[cfg(windows)]
            Conn::Pipe(f) => {
                (&*f).write_all(line.as_bytes())?;
                (&*f).write_all(b"\n")?;
                (&*f).flush()?;
            }
        }
        Ok(())
    }

    /// Split into a reader + writer sharing the connection. The reader
    /// stays concrete so the ack-phase read timeout can be lifted once
    /// the subscription is established (events are long-lived).
    fn split(self) -> Result<(SockReader, Box<dyn Write + Send>), ClientError> {
        match self {
            #[cfg(unix)]
            Conn::Unix(s) => {
                let read = s.try_clone()?;
                Ok((
                    SockReader::Unix(BufReader::new(read)),
                    Box::new(s) as Box<dyn Write + Send>,
                ))
            }
            #[cfg(windows)]
            Conn::Pipe(f) => {
                let read = f.try_clone()?;
                Ok((
                    SockReader::Pipe(BufReader::new(read)),
                    Box::new(f) as Box<dyn Write + Send>,
                ))
            }
        }
    }
}

/// Concrete read half of a connection (see `Conn::split`).
enum SockReader {
    #[cfg(unix)]
    Unix(BufReader<std::os::unix::net::UnixStream>),
    #[cfg(windows)]
    Pipe(BufReader<std::fs::File>),
}

impl SockReader {
    fn read_line(&mut self, buf: &mut String) -> std::io::Result<usize> {
        match self {
            #[cfg(unix)]
            SockReader::Unix(r) => r.read_line(buf),
            #[cfg(windows)]
            SockReader::Pipe(r) => r.read_line(buf),
        }
    }

    fn set_read_timeout(&self, dur: Option<Duration>) -> std::io::Result<()> {
        match self {
            #[cfg(unix)]
            SockReader::Unix(r) => r.get_ref().set_read_timeout(dur),
            #[cfg(windows)]
            SockReader::Pipe(_) => Ok(()),
        }
    }
}

fn parse_message(line: &str) -> Option<Value> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    serde_json::from_str(trimmed).ok()
}

/// Classify one response line addressed to `id`: Ok(result), Err(rpc),
/// or None when the line is not for this request.
fn classify_response(line: &str, id: &str) -> Option<Result<Value, ClientError>> {
    let message = parse_message(line)?;
    if message.get("id").and_then(|v| v.as_str()) != Some(id) {
        return None;
    }
    if let Some(err) = message.get("error") {
        return Some(Err(ClientError::Rpc(HerdrRpcError::from_value(err))));
    }
    Some(Ok(message.get("result").cloned().unwrap_or(Value::Null)))
}

fn is_timeout_error(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

/// One-shot request/response. Opens a connection, sends one JSON line,
/// resolves on the first response line with a matching `id`.
pub fn request(
    socket_path: &str,
    method: &str,
    params: Value,
    timeout_ms: u64,
) -> Result<Value, ClientError> {
    let mut conn = Conn::connect(socket_path)?;
    conn.set_read_timeout(Some(Duration::from_millis(timeout_ms)))?;
    let id = next_id("hp");
    let line = serde_json::to_string(&json!({"id": id, "method": method, "params": params}))
        .map_err(|e| ClientError::Malformed(e.to_string()))?;
    conn.write_line(&line)?;

    match conn {
        #[cfg(unix)]
        Conn::Unix(stream) => {
            let mut reader = BufReader::new(stream);
            read_response(&mut reader, &id, method, timeout_ms)
        }
        #[cfg(windows)]
        Conn::Pipe(file) => {
            // &File implements Read on Windows; the write half already
            // flushed. Read through a shared reference.
            let mut reader = BufReader::new(&file);
            read_response(&mut reader, &id, method, timeout_ms)
        }
    }
}

fn read_response<R: BufRead>(
    reader: &mut R,
    id: &str,
    method: &str,
    timeout_ms: u64,
) -> Result<Value, ClientError> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).map_err(|e| {
            if is_timeout_error(&e) {
                ClientError::Timeout(format!(
                    "herdr request {} timed out after {}ms",
                    method, timeout_ms
                ))
            } else {
                ClientError::Io(e.to_string())
            }
        })?;
        if n == 0 {
            return Err(ClientError::Closed(format!(
                "herdr socket closed before answering {}",
                method
            )));
        }
        if let Some(result) = classify_response(&line, id) {
            return result;
        }
    }
}

/// One pushed event envelope `{event, data}`.
#[derive(Debug, Clone)]
pub struct HerdrEvent {
    pub event: String,
    pub data: Value,
}

/// Long-lived `events.subscribe` connection. The subscription is
/// acknowledged (or failed) before `subscribe` returns; a reader thread
/// owns the socket afterwards. `recv`/`try_recv` yield pushed envelopes
/// until `close()` or stream end.
pub struct Subscription {
    // Mutex so `Subscription` is `Sync` (Receiver is Send but not Sync);
    // `recv`/`try_recv` take `&self` and callers may share across threads.
    rx: std::sync::Mutex<Receiver<Result<HerdrEvent, ClientError>>>,
    stop: Arc<AtomicBool>,
    started: Result<Value, ClientError>,
    /// Error frame addressed to this subscription after the ack
    /// (e.g. `events_lost`), when observed.
    terminal_error: Arc<std::sync::Mutex<Option<HerdrRpcError>>>,
}

impl Subscription {
    /// The acknowledgement result (already resolved at subscribe time).
    pub fn started(&self) -> Result<Value, ClientError> {
        match &self.started {
            Ok(v) => Ok(v.clone()),
            Err(e) => Err(match e {
                ClientError::Rpc(r) => ClientError::Rpc(HerdrRpcError {
                    code: r.code.clone(),
                    message: r.message.clone(),
                }),
                ClientError::Io(m) => ClientError::Io(m.clone()),
                ClientError::Timeout(m) => ClientError::Timeout(m.clone()),
                ClientError::Closed(m) => ClientError::Closed(m.clone()),
                ClientError::Malformed(m) => ClientError::Malformed(m.clone()),
            }),
        }
    }

    /// Blocking receive of the next event envelope; `None` at stream end.
    pub fn recv(&self) -> Option<Result<HerdrEvent, ClientError>> {
        self.rx.lock().ok()?.recv().ok()
    }

    /// Non-blocking receive.
    pub fn try_recv(&self) -> Option<Result<HerdrEvent, ClientError>> {
        self.rx.lock().ok()?.try_recv().ok()
    }

    /// Error frame addressed to this subscription after the ack, if any.
    pub fn terminal_error(&self) -> Option<HerdrRpcError> {
        self.terminal_error.lock().ok().and_then(|g| g.clone())
    }

    pub fn close(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.close();
    }
}

/// Open a long-lived `events.subscribe` connection and wait for the ack.
/// Mirrors TS `subscribe` + `await subscription.started`: connect errors,
/// ack timeouts, and error frames all surface as `Err` here.
pub fn subscribe(
    socket_path: &str,
    subscriptions: Value,
    timeout_ms: u64,
) -> Result<Subscription, ClientError> {
    let conn = Conn::connect(socket_path)?;
    conn.set_read_timeout(Some(Duration::from_millis(timeout_ms)))?;
    let id = next_id("hp-sub");
    let line = serde_json::to_string(&json!({
        "id": id,
        "method": "events.subscribe",
        "params": { "subscriptions": subscriptions },
    }))
    .map_err(|e| ClientError::Malformed(e.to_string()))?;

    // Read the ack on this thread (bounded by the socket read timeout);
    // event lines arriving before the ack are buffered and replayed.
    let (mut reader, mut writer) = conn.split()?;
    writer.write_all(line.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    drop(writer);

    let mut buffered: Vec<HerdrEvent> = Vec::new();
    #[allow(unused_assignments)]
    let mut started: Result<Value, ClientError> =
        Err(ClientError::Closed("subscription ended before ack".into()));
    let mut raw = String::new();
    loop {
        raw.clear();
        let n = reader.read_line(&mut raw).map_err(|e| {
            if is_timeout_error(&e) {
                ClientError::Timeout(format!(
                    "herdr events.subscribe timed out after {}ms",
                    timeout_ms
                ))
            } else {
                ClientError::Io(e.to_string())
            }
        })?;
        if n == 0 {
            return Err(ClientError::Closed(
                "herdr socket closed before subscription started".into(),
            ));
        }
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if message.get("id").and_then(|v| v.as_str()) == Some(id.as_str()) {
            if let Some(err) = message.get("error") {
                return Err(ClientError::Rpc(HerdrRpcError::from_value(err)));
            }
            started = Ok(message.get("result").cloned().unwrap_or(Value::Null));
            break;
        }
        if let Some(event) = message.get("event").and_then(|v| v.as_str()) {
            buffered.push(HerdrEvent {
                event: event.to_string(),
                data: message.get("data").cloned().unwrap_or(Value::Null),
            });
        }
    }
    // The ack has arrived; lift the read timeout — events are long-lived
    // and a quiet stream must never time out.
    let _ = reader.set_read_timeout(None);

    let (tx, rx) = channel::<Result<HerdrEvent, ClientError>>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let terminal_error = Arc::new(std::sync::Mutex::new(None::<HerdrRpcError>));
    let terminal_error2 = terminal_error.clone();

    thread::spawn(move || {
        for event in buffered {
            if tx.send(Ok(event)).is_err() {
                return;
            }
        }
        loop {
            if stop2.load(Ordering::Relaxed) {
                break;
            }
            raw.clear();
            match reader.read_line(&mut raw) {
                Ok(0) => {
                    let _ = tx.send(Err(ClientError::Closed("herdr socket closed".into())));
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    let err = ClientError::from(e);
                    let _ = tx.send(Err(err));
                    break;
                }
            }
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            let message: Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(_) => continue,
            };
            // Error frame addressed to this subscription (e.g. events_lost)
            // or any frame with an `error` and no event: record + end.
            if message.get("event").is_none() {
                if let Some(err) = message.get("error") {
                    let rpc = HerdrRpcError::from_value(err);
                    if let Ok(mut guard) = terminal_error2.lock() {
                        *guard = Some(HerdrRpcError {
                            code: rpc.code.clone(),
                            message: rpc.message.clone(),
                        });
                    }
                    let _ = tx.send(Err(ClientError::Rpc(rpc)));
                    break;
                }
                continue;
            }
            let event = message.get("event").and_then(|v| v.as_str()).unwrap_or("");
            let data = message.get("data").cloned().unwrap_or(Value::Null);
            if tx
                .send(Ok(HerdrEvent {
                    event: event.to_string(),
                    data,
                }))
                .is_err()
            {
                break;
            }
        }
    });

    Ok(Subscription {
        rx: std::sync::Mutex::new(rx),
        stop,
        started,
        terminal_error,
    })
}
