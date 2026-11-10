//! A scripted HTTP/1.1 server for provider tests.
//!
//! Each accepted connection serves exactly one request and takes the next
//! scripted reply. Every response is sent with `Connection: close`, so a
//! client never reuses a connection and each request is one connection.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

#[derive(Debug, Clone)]
pub enum Reply {
    /// Wait `delay`, then send this response.
    Respond {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        delay: Duration,
    },
    /// Close the connection after reading the request, without a response.
    Close,
    /// Never respond; record when the client closes the connection.
    Hang,
    /// Send a 200 with a chunked body that never ends, until writes fail.
    StreamForever,
}

impl Reply {
    pub fn json(status: u16, body: &serde_json::Value) -> Self {
        Self::Respond {
            status,
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: body.to_string().into_bytes(),
            delay: Duration::ZERO,
        }
    }

    pub fn status(status: u16, body: &str) -> Self {
        Self::Respond {
            status,
            headers: Vec::new(),
            body: body.as_bytes().to_vec(),
            delay: Duration::ZERO,
        }
    }

    #[must_use]
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        if let Self::Respond { headers, .. } = &mut self {
            headers.push((name.to_string(), value.to_string()));
        }
        self
    }
}

#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub at: Instant,
    pub method: String,
    pub path: String,
    /// Header names are lowercased.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RecordedRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("request body is JSON")
    }
}

#[derive(Default)]
pub struct HttpState {
    pub script: VecDeque<Reply>,
    pub requests: Vec<RecordedRequest>,
    pub connections: usize,
    /// Hanging connections the client closed.
    pub client_closed: usize,
    /// Body bytes written by `StreamForever` replies.
    pub streamed_bytes: u64,
    /// `StreamForever` replies whose writes failed because the client left.
    pub streams_ended: usize,
}

pub struct FakeHttp {
    pub addr: SocketAddr,
    pub state: Arc<Mutex<HttpState>>,
    pub changed: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeHttp {
    pub async fn start(script: impl IntoIterator<Item = Reply>) -> Self {
        Self::start_at("127.0.0.1:0".parse().expect("address"), script).await
    }

    /// Serve on a chosen address, such as a port that refused earlier.
    pub async fn start_at(addr: SocketAddr, script: impl IntoIterator<Item = Reply>) -> Self {
        let listener = TcpListener::bind(addr).await.expect("bind");
        let addr = listener.local_addr().expect("address");
        let state = Arc::new(Mutex::new(HttpState {
            script: script.into_iter().collect(),
            ..HttpState::default()
        }));
        let changed = Arc::new(Notify::new());
        let task = {
            let state = state.clone();
            let changed = changed.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    state.lock().expect("state").connections += 1;
                    tokio::spawn(serve(stream, state.clone(), changed.clone()));
                }
            })
        };
        Self {
            addr,
            state,
            changed,
            task,
        }
    }

    /// `http://127.0.0.1:<port>/v1`
    pub fn base_url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    pub fn state(&self) -> std::sync::MutexGuard<'_, HttpState> {
        self.state.lock().expect("state")
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.state().requests.clone()
    }

    /// Wait until `done` holds, or panic after `limit`.
    pub async fn wait_for(&self, limit: Duration, done: impl Fn(&HttpState) -> bool) {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let notified = self.changed.notified();
            if done(&self.state()) {
                return;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                assert!(done(&self.state()), "condition not reached in {limit:?}");
                return;
            }
        }
    }

    /// Stop accepting connections; the port then refuses new ones.
    pub fn stop(self) {
        self.task.abort();
    }
}

async fn serve(mut stream: TcpStream, state: Arc<Mutex<HttpState>>, changed: Arc<Notify>) {
    let Some(request) = read_request(&mut stream).await else {
        return;
    };
    let reply = {
        let mut state = state.lock().expect("state");
        state.requests.push(request);
        state.script.pop_front().unwrap_or_else(|| Reply::Respond {
            status: 500,
            headers: Vec::new(),
            body: b"fake server script exhausted".to_vec(),
            delay: Duration::ZERO,
        })
    };
    changed.notify_waiters();
    match reply {
        Reply::Respond {
            status,
            headers,
            body,
            delay,
        } => {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let mut head = format!("HTTP/1.1 {status} Fake\r\n");
            for (name, value) in headers {
                let _ = write!(head, "{name}: {value}\r\n");
            }
            let _ = write!(
                head,
                "Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes()).await;
            let _ = stream.write_all(&body).await;
            let _ = stream.shutdown().await;
        }
        Reply::Close => {
            let _ = stream.shutdown().await;
        }
        Reply::Hang => {
            let mut buffer = [0_u8; 1024];
            loop {
                match stream.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            state.lock().expect("state").client_closed += 1;
            changed.notify_waiters();
        }
        Reply::StreamForever => {
            let head = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                        Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
            if stream.write_all(head.as_bytes()).await.is_err() {
                return;
            }
            let chunk = vec![b' '; 16 << 10];
            let framed = [
                format!("{:x}\r\n", chunk.len()).into_bytes(),
                chunk,
                b"\r\n".to_vec(),
            ]
            .concat();
            loop {
                if stream.write_all(&framed).await.is_err() {
                    break;
                }
                state.lock().expect("state").streamed_bytes += (16 << 10) as u64;
            }
            state.lock().expect("state").streams_ended += 1;
            changed.notify_waiters();
        }
    }
}

async fn read_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
    let mut data = Vec::new();
    let mut buffer = [0_u8; 8192];
    let head_end = loop {
        if let Some(position) = data.windows(4).position(|window| window == b"\r\n\r\n") {
            break position;
        }
        if data.len() > 64 << 10 {
            return None;
        }
        let read = stream.read(&mut buffer).await.ok()?;
        if read == 0 {
            return None;
        }
        data.extend_from_slice(&buffer[..read]);
    };
    let head = String::from_utf8_lossy(&data[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split(' ');
    let method = request_line.next()?.to_string();
    let path = request_line.next()?.to_string();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect::<Vec<_>>();
    let length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = data[head_end + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut buffer).await.ok()?;
        if read == 0 {
            return None;
        }
        body.extend_from_slice(&buffer[..read]);
    }
    body.truncate(length);
    Some(RecordedRequest {
        at: Instant::now(),
        method,
        path,
        headers,
        body,
    })
}
