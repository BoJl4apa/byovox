//! Local IPC: one JSON object per line each way over a named pipe (Windows) or an
//! abstract-namespace Unix socket (Linux) via `interprocess`. Also the single-instance
//! check: if a daemon answers `status`, one is running.

use std::io::{BufRead, BufReader, Write};

use anyhow::{Context, Result};
use interprocess::local_socket::{GenericNamespaced, ListenerOptions, Stream, prelude::*};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Request {
    Toggle,
    Quit,
    Status,
    Last,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct Reply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

pub fn socket_name() -> String {
    "byovox.sock".to_string()
}

pub fn serve(name: &str, handler: impl Fn(Request) -> Reply + Send + 'static) -> Result<()> {
    let ns = name
        .to_ns_name::<GenericNamespaced>()
        .context("socket name")?;
    let listener = ListenerOptions::new()
        .name(ns)
        .create_sync()
        .context("binding IPC socket")?;
    std::thread::Builder::new()
        .name("byovox-ipc".into())
        .spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { continue };
                let mut line = String::new();
                if BufReader::new(&mut conn).read_line(&mut line).is_err() {
                    continue;
                }
                let reply = match serde_json::from_str::<Request>(line.trim()) {
                    Ok(req) => handler(req),
                    Err(e) => Reply {
                        ok: false,
                        error: Some(format!("bad request: {e}")),
                        ..Default::default()
                    },
                };
                let _ = writeln!(
                    conn,
                    "{}",
                    serde_json::to_string(&reply).unwrap_or_else(|_| "{\"ok\":false}".into())
                );
            }
        })
        .context("spawning IPC thread")?;
    Ok(())
}

pub fn send(name: &str, req: Request) -> Result<Reply> {
    send_raw(name, &format!("{}\n", serde_json::to_string(&req)?))
}

pub fn send_raw(name: &str, line: &str) -> Result<Reply> {
    let ns = name
        .to_ns_name::<GenericNamespaced>()
        .context("socket name")?;
    let mut conn = Stream::connect(ns).context("connecting to the byovox daemon")?;
    conn.write_all(line.as_bytes())?;
    let mut reply = String::new();
    BufReader::new(&mut conn).read_line(&mut reply)?;
    serde_json::from_str(reply.trim()).context("parsing daemon reply")
}

pub fn daemon_running(name: &str) -> bool {
    send(name, Request::Status).map(|r| r.ok).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name() -> String {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("byovox-test-{}-{nanos:x}-{seq}", std::process::id())
    }

    #[test]
    fn round_trips_multiline_text_and_reports_state() {
        let n = name();
        serve(&n, |req| match req {
            Request::Last => Reply {
                ok: true,
                text: Some("1. milk\n2. eggs".into()),
                ..Default::default()
            },
            Request::Status => Reply {
                ok: true,
                state: Some("idle".into()),
                ..Default::default()
            },
            _ => Reply {
                ok: false,
                error: Some("nope".into()),
                ..Default::default()
            },
        })
        .unwrap();
        assert!(daemon_running(&n));
        let last = send(&n, Request::Last).unwrap();
        assert_eq!(last.text.as_deref(), Some("1. milk\n2. eggs"));
        assert_eq!(
            send(&n, Request::Status).unwrap().state.as_deref(),
            Some("idle")
        );
        let denied = send(&n, Request::Quit).unwrap();
        assert!(!denied.ok);
    }

    #[test]
    fn unparseable_request_gets_ok_false() {
        let n = name();
        serve(&n, |_| Reply {
            ok: true,
            ..Default::default()
        })
        .unwrap();
        let reply = send_raw(&n, "not json\n").unwrap();
        assert!(!reply.ok);
    }

    #[test]
    fn no_daemon_means_not_running() {
        assert!(!daemon_running(&name()));
    }
}
