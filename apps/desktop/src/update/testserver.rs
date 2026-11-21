//! A tiny HTTP server for the updater's tests, on loopback ports the system assigns.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Binds port 0 and reads the port the system picked, so two test processes on one machine
/// never collide.
pub struct Ports;

impl Ports {
    pub fn lock() -> Ports {
        Ports
    }

    pub fn serve(&self, routes: Vec<(&str, Reply)>) -> TestServer {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind a loopback port");
        let port = listener.local_addr().unwrap().port();
        TestServer::start(listener, port, routes)
    }
}

#[derive(Clone, Debug)]
pub enum Reply {
    Body(Vec<u8>),
    /// A 302 to this `Location`.
    Redirect(String),
    Status(u16),
    /// 200 with no Content-Length and a body that never ends.
    Endless,
    /// The first reply for the first request, the second for every later one.
    Then(Box<Reply>, Box<Reply>),
}

type Routes = Arc<Mutex<HashMap<String, Reply>>>;
type Hits = Arc<Mutex<HashMap<String, usize>>>;

pub struct TestServer {
    pub port: u16,
    routes: Routes,
    hits: Hits,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl TestServer {
    fn start(listener: TcpListener, port: u16, routes: Vec<(&str, Reply)>) -> TestServer {
        listener.set_nonblocking(true).unwrap();
        let routes: Routes = Arc::new(Mutex::new(
            routes
                .into_iter()
                .map(|(p, r)| (p.to_string(), r))
                .collect(),
        ));
        let hits: Hits = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (routes, hits, stop) = (routes.clone(), hits.clone(), stop.clone());
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let (routes, hits, stop) = (routes.clone(), hits.clone(), stop.clone());
                            thread::spawn(move || handle(stream, routes, hits, stop));
                        }
                        Err(_) => thread::sleep(Duration::from_millis(5)),
                    }
                }
            })
        };
        TestServer {
            port,
            routes,
            hits,
            stop,
            thread: Some(thread),
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// The same server under another loopback host name, so another origin.
    pub fn url_via_localhost(&self, path: &str) -> String {
        format!("http://localhost:{}{path}", self.port)
    }

    pub fn set(&self, path: &str, reply: Reply) {
        self.routes.lock().unwrap().insert(path.to_string(), reply);
    }

    pub fn hits(&self, path: &str) -> usize {
        self.hits.lock().unwrap().get(path).copied().unwrap_or(0)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn handle(mut stream: TcpStream, routes: Routes, hits: Hits, stop: Arc<AtomicBool>) {
    let _ = stream.set_nonblocking(false);
    // A test double on loopback. A read that times out or fails returns and drops the
    // connection (never read from again), as Windows requires of a socket whose receive
    // timed out, so the timeout cannot lose bytes that a later read would need.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut request = Vec::new();
    let mut buf = [0u8; 1024];
    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => request.extend_from_slice(&buf[..n]),
        }
    }
    let line = String::from_utf8_lossy(&request);
    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
    let count = {
        let mut hits = hits.lock().unwrap();
        let n = hits.entry(path.clone()).or_default();
        *n += 1;
        *n
    };
    let mut reply = routes.lock().unwrap().get(&path).cloned();
    if let Some(Reply::Then(first, later)) = reply {
        reply = Some(if count == 1 { *first } else { *later });
    }
    let _ = match reply {
        None => stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
        Some(Reply::Status(code)) => stream.write_all(
            format!("HTTP/1.1 {code} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes(),
        ),
        Some(Reply::Redirect(to)) => stream.write_all(
            format!("HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        ),
        Some(Reply::Body(body)) => stream
            .write_all(
                format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len())
                    .as_bytes(),
            )
            .and_then(|_| stream.write_all(&body)),
        Some(Reply::Then(..)) => Ok(()),
        Some(Reply::Endless) => {
            let chunk = [b'x'; 8192];
            let mut r = stream.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n");
            while r.is_ok() && !stop.load(Ordering::Relaxed) {
                r = stream.write_all(&chunk);
            }
            r
        }
    };
}
