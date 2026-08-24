//! Test doubles for the HTTP boundary: a one-thread server that records the raw
//! requests it receives and replays canned (or deliberately malformed) responses.
//! `fakes` holds the in-process doubles for the platform traits — capture, layout,
//! transcriber, polisher, inject rungs and indicator — that the pipeline is driven with.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

/// What the server does with one accepted connection.
enum Reply {
    /// Read and record the request, then write this exact response text.
    Raw(String),
    /// Close the connection without reading or answering it.
    Close,
}

pub struct MockServer {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl MockServer {
    /// Answers every request with `status` and `body` (JSON), recording the raw request.
    pub fn start(status: u16, body: &str) -> MockServer {
        Self::serve(vec![Reply::Raw(Self::response(status, body))])
    }

    /// Answers every request with this exact response text — for replies a well-formed
    /// server would not produce, such as a `Content-Length` longer than the body sent.
    pub fn start_raw(response: &str) -> MockServer {
        Self::serve(vec![Reply::Raw(response.to_string())])
    }

    /// Closes the first connection without reading or answering it, then answers
    /// normally: the transport failure a single retry is meant to survive.
    pub fn start_closing_first(status: u16, body: &str) -> MockServer {
        Self::serve(vec![Reply::Close, Reply::Raw(Self::response(status, body))])
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    fn response(status: u16, body: &str) -> String {
        format!(
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// Serves connection *i* with `replies[i]`; the last entry repeats forever.
    fn serve(replies: Vec<Reply>) -> MockServer {
        assert!(
            !replies.is_empty(),
            "a mock server needs at least one reply"
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests: Arc<Mutex<Vec<String>>> = Default::default();
        let recorded = requests.clone();
        std::thread::spawn(move || {
            for (i, stream) in listener.incoming().enumerate() {
                let Ok(mut s) = stream else { break };
                // Dropping `s` unread closes the connection under the client.
                let Reply::Raw(response) = &replies[i.min(replies.len() - 1)] else {
                    continue;
                };
                let mut raw = Vec::new();
                let mut buf = [0u8; 4096];
                // Read headers, then exactly Content-Length body bytes.
                let mut header_end = None;
                while header_end.is_none() {
                    let Ok(n) = s.read(&mut buf) else { break };
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
                        let (name, value) = l.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim())
                    })
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                while raw.len() < he + len {
                    let Ok(n) = s.read(&mut buf) else { break };
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..n]);
                }
                recorded
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&raw).to_string());
                let _ = s.write_all(response.as_bytes());
            }
        });
        MockServer { url, requests }
    }
}

pub mod fakes {
    use std::sync::{Arc, Mutex};

    use crate::audio::Audio;
    use crate::capture::Capture;
    use crate::indicator::{Indicator, IndicatorState};
    use crate::inject::Inject;
    use crate::lang::{Lang, SttLanguage};
    use crate::layout::Layout;
    use crate::polish::Polisher;
    use crate::stt::Transcriber;

    pub struct FakeCapture {
        pub samples: usize,
        pub started: usize,
    }
    impl Capture for FakeCapture {
        fn start(&mut self) -> Result<(), String> {
            self.started += 1;
            Ok(())
        }
        fn stop(&mut self) -> Result<Audio, String> {
            Ok(Audio {
                samples: vec![1000; self.samples],
            })
        }
    }

    pub struct FakeLayout(pub Option<Lang>);
    impl Layout for FakeLayout {
        fn current(&self) -> Option<Lang> {
            self.0
        }
    }

    /// One recorded `transcribe` call: the language's form fields, and the prompt.
    pub type TranscribeCall = (Vec<(&'static str, String)>, Option<String>);

    #[derive(Clone)]
    pub struct FakeTranscriber {
        pub result: Result<String, String>,
        pub calls: Arc<Mutex<Vec<TranscribeCall>>>,
    }
    impl FakeTranscriber {
        pub fn ok(text: &str) -> Self {
            Self {
                result: Ok(text.into()),
                calls: Default::default(),
            }
        }
        pub fn err(e: &str) -> Self {
            Self {
                result: Err(e.into()),
                calls: Default::default(),
            }
        }
    }
    impl Transcriber for FakeTranscriber {
        fn transcribe(
            &self,
            _wav: &[u8],
            language: &SttLanguage,
            prompt: Option<&str>,
        ) -> Result<String, String> {
            self.calls
                .lock()
                .unwrap()
                .push((language.form_fields(), prompt.map(str::to_string)));
            self.result.clone()
        }
    }

    #[derive(Clone)]
    pub struct FakePolisher {
        pub result: Result<String, String>,
        pub calls: Arc<Mutex<Vec<String>>>,
    }
    impl FakePolisher {
        pub fn ok(text: &str) -> Self {
            Self {
                result: Ok(text.into()),
                calls: Default::default(),
            }
        }
        pub fn err(e: &str) -> Self {
            Self {
                result: Err(e.into()),
                calls: Default::default(),
            }
        }
    }
    impl Polisher for FakePolisher {
        fn polish(&self, raw: &str) -> Result<String, String> {
            self.calls.lock().unwrap().push(raw.to_string());
            self.result.clone()
        }
    }

    #[derive(Clone)]
    pub struct FakeInject {
        pub name: &'static str,
        pub fail: bool,
        pub texts: Arc<Mutex<Vec<String>>>,
    }
    impl FakeInject {
        pub fn new(name: &'static str, fail: bool) -> Self {
            Self {
                name,
                fail,
                texts: Default::default(),
            }
        }
    }
    impl Inject for FakeInject {
        fn name(&self) -> &'static str {
            self.name
        }
        fn inject(&mut self, text: &str) -> Result<(), String> {
            if self.fail {
                return Err(format!("{} failed", self.name));
            }
            self.texts.lock().unwrap().push(text.to_string());
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    pub struct FakeIndicator(pub Arc<Mutex<Vec<IndicatorState>>>);
    impl Indicator for FakeIndicator {
        fn set(&mut self, state: IndicatorState) {
            self.0.lock().unwrap().push(state);
        }
    }
}
