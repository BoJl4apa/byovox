//! Test doubles: a one-thread HTTP server that records requests, and fakes for the
//! platform traits (added in Task 7).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

pub struct MockServer {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl MockServer {
    /// Answers every request with `status` and `body` (JSON), recording the raw request.
    pub fn start(status: u16, body: &'static str) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests: Arc<Mutex<Vec<String>>> = Default::default();
        let recorded = requests.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut raw = Vec::new();
                let mut buf = [0u8; 4096];
                // Read headers, then exactly Content-Length body bytes.
                let mut header_end = None;
                while header_end.is_none() {
                    let n = s.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..n]);
                    header_end = raw.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4);
                }
                let Some(he) = header_end else { continue };
                let head = String::from_utf8_lossy(&raw[..he]).to_string();
                let len: usize = head
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("Content-Length: ")
                            .or_else(|| l.strip_prefix("content-length: "))
                    })
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                while raw.len() < he + len {
                    let n = s.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..n]);
                }
                recorded
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&raw).to_string());
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });
        MockServer { url, requests }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}
