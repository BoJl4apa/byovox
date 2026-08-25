//! Local IPC: one JSON object per line each way over a named pipe (Windows) or an
//! abstract-namespace Unix socket (Linux) via `interprocess`. Also the single-instance
//! check: if a daemon answers `status`, one is running.

use std::io::{BufRead, BufReader, Write};
use std::sync::{Arc, Mutex};

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

#[derive(Serialize, Deserialize, Default, Clone)]
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

/// Hand-written so the transcript in `text` can never reach a log through `{:?}`.
impl std::fmt::Debug for Reply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reply")
            .field("ok", &self.ok)
            .field("error", &self.error)
            .field("text", &self.text.as_ref().map(|_| "<redacted>"))
            .field("state", &self.state)
            .field("last_error", &self.last_error)
            .finish()
    }
}

/// Per-user: the Windows pipe namespace is machine-global, so two accounts on one box would
/// otherwise fight over a single name.
pub fn socket_name() -> String {
    let user = std::env::var(if cfg!(windows) { "USERNAME" } else { "USER" })
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| "default".to_string());
    format!("byovox-{user}.sock")
}

/// Spawn the listener thread; `Err` when the name is already taken, which is the
/// single-instance guard.
///
/// One request per connection: the first line is the request, any bytes after it are
/// discarded, and the connection closes once the one reply line is written. Every accepted
/// connection gets its own thread, so a client that never writes — or never drains its reply —
/// blocks only itself; a mutex serialises the handler calls, so the handler itself still sees
/// one request at a time.
///
/// A `quit` handler must return its reply *before* it triggers process exit, or the client
/// sees EOF instead of an answer.
pub fn serve(name: &str, handler: impl Fn(Request) -> Reply + Send + 'static) -> Result<()> {
    let ns = name
        .to_ns_name::<GenericNamespaced>()
        .context("socket name")?;
    let listener = ListenerOptions::new()
        .name(ns)
        .create_sync()
        .context("binding IPC socket")?;
    let handler = Arc::new(Mutex::new(handler));
    std::thread::Builder::new()
        .name("byovox-ipc".into())
        .spawn(move || {
            for conn in listener.incoming() {
                let mut conn = match conn {
                    Ok(conn) => conn,
                    Err(e) => {
                        tracing::debug!(error = %e, "ipc accept error");
                        continue;
                    }
                };
                let handler = Arc::clone(&handler);
                let spawned = std::thread::Builder::new()
                    .name("byovox-ipc-conn".into())
                    .spawn(move || {
                        let mut line = String::new();
                        if let Err(e) = BufReader::new(&mut conn).read_line(&mut line) {
                            tracing::debug!(error = %e, "ipc read error");
                            return;
                        }
                        let reply = match serde_json::from_str::<Request>(line.trim()) {
                            // Poison recovery: one panicking handler must not deny every
                            // later request.
                            Ok(req) => (*handler.lock().unwrap_or_else(|p| p.into_inner()))(req),
                            Err(e) => Reply {
                                ok: false,
                                error: Some(format!("bad request: {e}")),
                                ..Default::default()
                            },
                        };
                        let encoded = serde_json::to_string(&reply)
                            .unwrap_or_else(|_| "{\"ok\":false}".into());
                        if let Err(e) = writeln!(conn, "{encoded}") {
                            tracing::debug!(error = %e, "ipc write error");
                        }
                    });
                if let Err(e) = spawned {
                    tracing::debug!(error = %e, "ipc connection thread error");
                }
            }
        })
        .context("spawning IPC thread")?;
    Ok(())
}

pub fn send(name: &str, req: Request) -> Result<Reply> {
    send_raw(name, &format!("{}\n", serde_json::to_string(&req)?))
}

pub(crate) fn send_raw(name: &str, line: &str) -> Result<Reply> {
    let ns = name
        .to_ns_name::<GenericNamespaced>()
        .context("socket name")?;
    let mut conn = Stream::connect(ns).context("connecting to the byovox daemon")?;
    conn.write_all(line.as_bytes())?;
    if !line.ends_with('\n') {
        conn.write_all(b"\n")?;
    }
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
        let error = reply.error.expect("a rejected request must say why");
        assert!(!error.is_empty());
    }

    #[test]
    fn no_daemon_means_not_running() {
        assert!(!daemon_running(&name()));
    }

    #[test]
    fn a_silent_client_blocks_only_itself() {
        let n = name();
        serve(&n, |_| Reply {
            ok: true,
            state: Some("idle".into()),
            ..Default::default()
        })
        .unwrap();

        let silent = n.clone();
        let (connected_tx, connected_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let ns = silent
                .to_ns_name::<GenericNamespaced>()
                .expect("silent client name");
            let conn = Stream::connect(ns).expect("silent client connect");
            let _ = connected_tx.send(());
            std::thread::sleep(std::time::Duration::from_secs(3));
            drop(conn);
        });
        connected_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the silent client never connected");

        let live = n.clone();
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = reply_tx.send(send(&live, Request::Status).map(|r| r.ok));
        });
        let ok = reply_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("a silent client wedged the listener")
            .expect("the status request failed");
        assert!(ok);
    }

    #[test]
    fn socket_name_is_per_user() {
        let s = socket_name();
        assert!(s.starts_with("byovox-"), "{s}");
        assert!(s.ends_with(".sock"), "{s}");
        assert!(s.len() > "byovox-.sock".len(), "{s}");
    }

    #[test]
    fn wire_format_is_pinned() {
        assert_eq!(
            serde_json::to_string(&Request::Toggle).unwrap(),
            r#"{"cmd":"toggle"}"#
        );
        assert_eq!(
            serde_json::to_string(&Reply {
                ok: true,
                ..Default::default()
            })
            .unwrap(),
            r#"{"ok":true}"#
        );
        let sent = Reply {
            ok: true,
            text: Some("1. milk\n2. eggs".into()),
            ..Default::default()
        };
        let back: Reply = serde_json::from_str(&serde_json::to_string(&sent).unwrap()).unwrap();
        assert!(back.ok);
        assert_eq!(back.text.as_deref(), Some("1. milk\n2. eggs"));
        assert!(back.error.is_none());
    }

    #[test]
    fn debug_redacts_transcript_text() {
        let reply = Reply {
            ok: true,
            text: Some("my bank password is hunter2".into()),
            state: Some("idle".into()),
            ..Default::default()
        };
        let rendered = format!("{reply:?}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(rendered.contains(r#"Some("<redacted>")"#), "{rendered}");
        assert!(rendered.contains("idle"), "{rendered}");
    }
}
