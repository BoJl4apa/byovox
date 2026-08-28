//! Speech-to-text client: one multipart POST to `{base_url}/audio/transcriptions`.
//!
//! Depends on `multipart` and `lang::SttLanguage`. Produces the trimmed transcript and
//! whisper's own no-speech score for it. Errors are strings the pipeline logs and shows;
//! the single retry fires only when no response came back at all — a received status,
//! however bad, is never retried.

use std::time::Duration;

use crate::lang::SttLanguage;
use crate::multipart::Multipart;

/// One transcription: the trimmed text, and how sure whisper is that the clip held no speech
/// at all. `None` when the reply carried no `segments` — a server flavour that does not score
/// its output leaves the gate nothing to judge, which is the old behaviour rather than a
/// silent zero.
#[derive(Debug, Clone, PartialEq)]
pub struct Transcript {
    pub text: String,
    /// The strongest `no_speech_prob` across the reply's segments: one hallucinated segment in
    /// an otherwise silent hold is exactly what this is here to catch, so the maximum decides.
    pub no_speech_prob: Option<f32>,
}

pub trait Transcriber: Send {
    fn transcribe(
        &self,
        wav: &[u8],
        language: &SttLanguage,
        prompt: Option<&str>,
    ) -> Result<Transcript, String>;
}

pub struct SttClient {
    url: String,
    model: String,
    api_key: Option<String>,
    /// Ask for `verbose_json` and read the scores out of it. Only a caller that will act on
    /// a score turns this on — see `new`.
    scored: bool,
    agent: ureq::Agent,
}

impl SttClient {
    /// `scored` asks the server for `verbose_json`, the one format carrying
    /// `segments[].no_speech_prob`. It is on only when something downstream will use the
    /// number (`stt.no_speech_threshold > 0`), because not every OpenAI-compatible server
    /// accepts that format — `gpt-4o-transcribe` answers `json` and `text` alone and 400s on
    /// the rest. With it off the request is a plain `json` one and the score is `None`, so
    /// turning the gate off restores a wire every such server takes.
    pub fn new(
        base_url: &str,
        model: &str,
        api_key: Option<String>,
        timeout: Duration,
        scored: bool,
    ) -> SttClient {
        let config = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(5)))
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .build();
        SttClient {
            url: format!("{}/audio/transcriptions", base_url.trim_end_matches('/')),
            model: model.to_string(),
            api_key,
            scored,
            agent: config.into(),
        }
    }

    fn post_once(&self, content_type: &str, body: &[u8]) -> Attempt {
        let mut req = self
            .agent
            .post(&self.url)
            .header("Content-Type", content_type);
        if let Some(k) = &self.api_key {
            req = req.header("Authorization", &format!("Bearer {k}"));
        }
        let mut resp = match req.send(body) {
            Ok(resp) => resp,
            Err(e) => return Attempt::SendFailed(e.to_string()),
        };
        let status = resp.status().as_u16();
        match resp.body_mut().read_to_string() {
            Ok(body) => Attempt::Response { status, body },
            Err(e) => Attempt::BodyUnreadable {
                status,
                error: e.to_string(),
            },
        }
    }
}

/// The outcome of one POST attempt. Only `SendFailed` may be retried: once a status
/// has come back the audio reached the server, so replaying the request risks a second
/// transcription — and would throw away the status the caller needs to see.
enum Attempt {
    /// A response arrived and its body was read.
    Response { status: u16, body: String },
    /// A status arrived but the body could not be read.
    BodyUnreadable { status: u16, error: String },
    /// No response arrived at all.
    SendFailed(String),
}

/// The leading 200 characters of a response body, for error messages.
fn body_prefix(body: &str) -> String {
    body.chars().take(200).collect()
}

/// The strongest `no_speech_prob` in a `verbose_json` reply, `None` when the reply carries no
/// `segments` — absent or null are the same answer, a list of nothing — and when none of the
/// segments is scored. A `segments` that is some other non-list, or a score that is not a
/// number, is a reply this client does not understand: it fails loudly rather than handing
/// the gate a silent `None` that would look exactly like an unscored server.
fn max_no_speech_prob(v: &serde_json::Value, body: &str) -> Result<Option<f32>, String> {
    let Some(segments) = v.get("segments").filter(|s| !s.is_null()) else {
        return Ok(None);
    };
    let segments = segments.as_array().ok_or_else(|| {
        format!(
            "stt response segments is not an array: {}",
            body_prefix(body)
        )
    })?;
    let mut max: Option<f32> = None;
    for s in segments {
        let Some(p) = s.get("no_speech_prob") else {
            continue;
        };
        let p = p.as_f64().ok_or_else(|| {
            format!(
                "stt response no_speech_prob is not a number: {}",
                body_prefix(body)
            )
        })? as f32;
        max = Some(max.map_or(p, |m| m.max(p)));
    }
    Ok(max)
}

impl Transcriber for SttClient {
    fn transcribe(
        &self,
        wav: &[u8],
        language: &SttLanguage,
        prompt: Option<&str>,
    ) -> Result<Transcript, String> {
        let mut m = Multipart::new();
        m.file("file", "clip.wav", "audio/wav", wav);
        m.text("model", &self.model);
        // `verbose_json` is the only format that carries `segments[].no_speech_prob`, which
        // is what tells a silent hold from a real one — and the only reason to ask for a
        // format some servers refuse. Asked for only when a score is wanted.
        m.text(
            "response_format",
            if self.scored { "verbose_json" } else { "json" },
        );
        for (name, value) in language.form_fields() {
            m.text(name, &value);
        }
        if let Some(p) = prompt.filter(|p| !p.trim().is_empty()) {
            m.text("prompt", p);
        }
        let (content_type, body) = m.finish();

        // One retry, and only when no response came back at all.
        let attempt = match self.post_once(&content_type, &body) {
            Attempt::SendFailed(first) => {
                tracing::warn!(error = %first, "stt transport error, retrying once");
                self.post_once(&content_type, &body)
            }
            answered => answered,
        };
        let (status, text) = match attempt {
            Attempt::Response { status, body } => (status, body),
            // No colon straight after the status: `check::strip_body` cuts everything past
            // `HTTP <status>: ` as response body, and here what follows is the io cause,
            // which is the whole diagnosis.
            Attempt::BodyUnreadable { status, error } => {
                return Err(format!("stt HTTP {status} body unreadable: {error}"));
            }
            // The stage names itself: `pipeline::summary` keeps only the head before the
            // first colon, so a bare `transport` is all the tray tooltip, the tray status
            // line and `byovox status` would say about the commonest failure there is —
            // server down, wrong port, VPN off.
            Attempt::SendFailed(e) => return Err(format!("stt transport: {e}")),
        };
        if !(200..300).contains(&status) {
            return Err(format!("stt HTTP {status}: {}", body_prefix(&text)));
        }
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("stt JSON: {e}"))?;
        let transcript = v
            .get("text")
            .and_then(|t| t.as_str())
            .ok_or_else(|| format!("stt response has no text field: {}", body_prefix(&text)))?;
        // whisper breaks `text` at segment boundaries — pauses, not spoken line breaks —
        // so the lines are joined on a space before polish, the log or `byovox last` see it.
        let joined = transcript
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        Ok(Transcript {
            text: joined,
            // An unscored client reads no score at all: a server that answers a `json`
            // request with segments anyway cannot reach the gate, and no shape it puts in
            // them can fail a request whose scores nothing will read.
            no_speech_prob: if self.scored {
                max_no_speech_prob(&v, &text)?
            } else {
                None
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::{Lang, SttLanguage};
    use crate::testutil::MockServer;
    use std::net::TcpListener;
    use std::time::Duration;

    /// A client with the gate on, which is the shipped default.
    fn client(url: &str) -> SttClient {
        SttClient::new(url, "whisper-1", None, Duration::from_secs(5), true)
    }

    /// A client with `stt.no_speech_threshold = 0`: nothing downstream reads a score.
    fn unscored_client(url: &str) -> SttClient {
        SttClient::new(url, "whisper-1", None, Duration::from_secs(5), false)
    }

    /// The `Authorization` value, found case-insensitively by name (RFC 9110 header
    /// names are case-insensitive, and ureq 3 sends them lowercased).
    fn auth_header(req: &str) -> Option<String> {
        req.lines().find_map(|l| {
            let (name, value) = l.split_once(':')?;
            name.eq_ignore_ascii_case("authorization")
                .then(|| value.trim().to_string())
        })
    }

    #[test]
    fn explicit_language_and_prompt_are_sent_and_text_is_trimmed() {
        let srv = MockServer::start(200, r#"{"text":" hello world \n"}"#);
        let he = SttLanguage::Explicit(Lang::parse("he").unwrap());
        let out = client(&format!("{}/v1", srv.url()))
            .transcribe(b"RIFFxxxx", &he, Some("Glossary: Acme"))
            .unwrap();
        assert_eq!(out.text, "hello world");
        let req = srv.requests().remove(0);
        assert!(req.starts_with("POST /v1/audio/transcriptions "), "{req}");
        assert!(req.contains("name=\"language\"\r\n\r\nhe\r\n"));
        assert!(req.contains("name=\"prompt\"\r\n\r\nGlossary: Acme\r\n"));
        assert!(req.contains("name=\"response_format\"\r\n\r\nverbose_json\r\n"));
        assert!(req.contains("name=\"model\"\r\n\r\nwhisper-1\r\n"));
        // The file part, framed exactly: disposition, type, then the raw payload bytes.
        assert!(
            req.contains(
                "name=\"file\"; filename=\"clip.wav\"\r\nContent-Type: audio/wav\r\n\r\nRIFFxxxx\r\n"
            ),
            "{req}"
        );
        assert_eq!(auth_header(&req), None, "{req}");
    }

    /// Not every OpenAI-compatible server takes `verbose_json` — `gpt-4o-transcribe` rejects
    /// it outright — so the format byovox asks for has to follow the one knob that decides
    /// whether the score is wanted. With the gate off the request is the plain `json` one
    /// every such server accepts, and nothing is read out of the reply's segments even if
    /// the server volunteers them.
    #[test]
    fn the_gate_being_off_asks_for_plain_json_and_reads_no_score() {
        let srv = MockServer::start(
            200,
            r#"{"text":"hello","segments":[{"id":0,"no_speech_prob":0.9}]}"#,
        );
        let out = unscored_client(srv.url())
            .transcribe(b"RIFF", &SttLanguage::Auto { candidates: vec![] }, None)
            .unwrap();
        assert_eq!(out.text, "hello");
        assert_eq!(out.no_speech_prob, None);
        let req = srv.requests().remove(0);
        assert!(
            req.contains("name=\"response_format\"\r\n\r\njson\r\n"),
            "{req}"
        );
        assert!(!req.contains("verbose_json"), "{req}");
    }

    /// The reply shapes that are a loud error for a scoring client are not even looked at by
    /// an unscored one: the score it would refuse to guess at is a score nothing will read.
    #[test]
    fn the_gate_being_off_survives_a_reply_a_scoring_client_would_refuse() {
        let srv = MockServer::start(
            200,
            r#"{"text":"hello","segments":[{"id":0,"no_speech_prob":"high"}]}"#,
        );
        let out = unscored_client(srv.url())
            .transcribe(b"RIFF", &SttLanguage::Auto { candidates: vec![] }, None)
            .unwrap();
        assert_eq!(out.text, "hello");
        assert_eq!(out.no_speech_prob, None);
    }

    #[test]
    fn auto_sends_candidates_and_no_language_field() {
        let srv = MockServer::start(200, r#"{"text":"x"}"#);
        let auto = SttLanguage::Auto {
            candidates: vec![Lang::parse("en").unwrap(), Lang::parse("ru").unwrap()],
        };
        client(srv.url()).transcribe(b"RIFF", &auto, None).unwrap();
        let req = srv.requests().remove(0);
        assert!(req.starts_with("POST /audio/transcriptions "), "{req}");
        assert!(req.contains("name=\"language_candidates\"\r\n\r\nen,ru\r\n"));
        assert!(!req.contains("name=\"language\"\r\n"));
        assert!(!req.contains("name=\"prompt\""));
    }

    #[test]
    fn bearer_header_when_key_given() {
        let srv = MockServer::start(200, r#"{"text":"x"}"#);
        let c = SttClient::new(
            srv.url(),
            "m",
            Some("tok".into()),
            Duration::from_secs(5),
            true,
        );
        c.transcribe(b"RIFF", &SttLanguage::Auto { candidates: vec![] }, None)
            .unwrap();
        // ureq 3 writes header names lowercased (`http::HeaderName`); values keep their case.
        let req = srv.requests().remove(0);
        assert_eq!(auth_header(&req).as_deref(), Some("Bearer tok"), "{req}");
    }

    #[test]
    fn http_error_is_an_error_with_status_and_no_retry() {
        let srv = MockServer::start(500, r#"{"error":"boom"}"#);
        let err = client(srv.url())
            .transcribe(b"RIFF", &SttLanguage::Auto { candidates: vec![] }, None)
            .unwrap_err();
        assert!(err.contains("500"), "{err}");
        assert_eq!(srv.requests().len(), 1, "4xx/5xx must not be retried");
    }

    /// The commonest failure there is, and `pipeline::summary` keeps only the head before
    /// the first colon — so the stage has to be in that head or every human surface says
    /// nothing but `transport`.
    #[test]
    fn connection_refused_is_an_error_that_names_the_stage() {
        // Bind a port, learn it, then drop the listener: nothing is listening there, so
        // the connection is refused outright instead of waiting on a dropped SYN.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let c = client(&format!("http://{addr}"));
        let err = c
            .transcribe(b"RIFF", &SttLanguage::Auto { candidates: vec![] }, None)
            .unwrap_err();
        assert!(err.starts_with("stt transport: "), "{err}");
    }

    #[test]
    fn a_truncated_body_keeps_the_status_and_is_not_retried() {
        // Promises 20 body bytes, sends none, closes: the status arrived, the body cannot
        // be read. Replaying would re-send the audio and lose the 500.
        let srv = MockServer::start_raw(
            "HTTP/1.1 500 X\r\nContent-Length: 20\r\nConnection: close\r\n\r\n",
        );
        let err = client(srv.url())
            .transcribe(b"RIFF", &SttLanguage::Auto { candidates: vec![] }, None)
            .unwrap_err();
        assert!(err.contains("500"), "{err}");
        // The io cause is the diagnosis and there is no response body to hide, so the
        // status is not followed by a colon — see `check::strip_body`.
        assert!(err.starts_with("stt HTTP 500 body unreadable: "), "{err}");
        assert_eq!(
            srv.requests().len(),
            1,
            "a received status must not be retried"
        );
    }

    #[test]
    fn a_send_failure_is_retried_once_and_succeeds() {
        // First connection is closed unread — no response, so the retry is allowed.
        let srv = MockServer::start_closing_first(200, r#"{"text":"second try"}"#);
        let out = client(srv.url())
            .transcribe(b"RIFF", &SttLanguage::Auto { candidates: vec![] }, None)
            .unwrap();
        assert_eq!(out.text, "second try");
        assert_eq!(
            srv.requests().len(),
            1,
            "only the retry's request is read and recorded"
        );
    }

    #[test]
    fn a_2xx_without_a_text_field_is_an_error() {
        let srv = MockServer::start(200, r#"{"error":"no speech detected"}"#);
        let err = client(srv.url())
            .transcribe(b"RIFF", &SttLanguage::Auto { candidates: vec![] }, None)
            .unwrap_err();
        assert!(err.contains("stt response has no text field"), "{err}");
        assert!(err.contains("no speech detected"), "{err}");
    }

    /// whisper breaks its `text` at segment boundaries — pauses, not spoken line breaks.
    /// They are joined on a space here, before polish, the capture log and `byovox last`
    /// see the transcript (#9).
    #[test]
    fn segment_line_breaks_are_joined_on_a_space() {
        let srv = MockServer::start(
            200,
            r#"{"text":" first thing \n second thing\r\n\nthird "}"#,
        );
        let out = client(srv.url())
            .transcribe(b"RIFF", &SttLanguage::Auto { candidates: vec![] }, None)
            .unwrap();
        assert_eq!(out.text, "first thing second thing third");
    }

    #[test]
    fn an_empty_text_field_is_an_empty_transcript() {
        let srv = MockServer::start(200, r#"{"text":""}"#);
        let out = client(srv.url())
            .transcribe(b"RIFF", &SttLanguage::Auto { candidates: vec![] }, None)
            .unwrap();
        assert_eq!(out.text, "");
    }

    /// A `verbose_json` reply, as whisper answers a hold that started with a word and ended
    /// in silence: the loudest segment scores near zero, the silent one near one. The gate
    /// has to see the silence, so the maximum is what comes back.
    #[test]
    fn the_strongest_segment_score_is_what_comes_back() {
        let srv = MockServer::start(
            200,
            r#"{"task":"transcribe","language":"en","duration":1.4,
                "text":" Thank you for watching! ",
                "segments":[
                  {"id":0,"start":0.0,"end":0.7,"text":"Thank you","no_speech_prob":0.04},
                  {"id":1,"start":0.7,"end":1.4,"text":" for watching!","no_speech_prob":0.74}
                ]}"#,
        );
        let out = client(srv.url())
            .transcribe(b"RIFF", &SttLanguage::Auto { candidates: vec![] }, None)
            .unwrap();
        assert_eq!(out.text, "Thank you for watching!");
        assert_eq!(out.no_speech_prob, Some(0.74));
    }

    /// A server that answers `verbose_json` with no segments scores nothing, and an unscored
    /// reply must reach the pipeline as "unknown" rather than as a confident zero. A null
    /// `segments` is that same well-formed answer — a list of nothing — and refusing it would
    /// fail every dictation from such a server over a reply nothing is wrong with.
    #[test]
    fn a_reply_without_segments_carries_no_score() {
        for body in [
            r#"{"text":"hello","language":"en"}"#,
            r#"{"text":"hello","segments":null}"#,
            r#"{"text":"hello","segments":[]}"#,
            // Segments that carry no score at all: nothing was measured.
            r#"{"text":"hello","segments":[{"id":0,"text":"hello"}]}"#,
        ] {
            let srv = MockServer::start(200, body);
            let out = client(srv.url())
                .transcribe(b"RIFF", &SttLanguage::Auto { candidates: vec![] }, None)
                .unwrap();
            assert_eq!(out.text, "hello", "{body}");
            assert_eq!(out.no_speech_prob, None, "{body}");
        }
    }

    /// The inner null stays loud: a segment that exists and carries an unreadable score is a
    /// score this client cannot read, not an absent one.
    #[test]
    fn a_null_score_inside_a_segment_is_still_an_error() {
        let srv = MockServer::start(
            200,
            r#"{"text":"x","segments":[{"id":0,"no_speech_prob":null}]}"#,
        );
        let err = client(srv.url())
            .transcribe(b"RIFF", &SttLanguage::Auto { candidates: vec![] }, None)
            .unwrap_err();
        assert!(
            err.starts_with("stt response no_speech_prob is not a number: "),
            "{err}"
        );
    }

    /// A score this client cannot read is a reply it does not understand. Skipping it would
    /// be indistinguishable from an unscored server, and the gate would silently stop gating.
    #[test]
    fn a_non_numeric_segment_score_is_an_error() {
        let srv = MockServer::start(
            200,
            r#"{"text":"x","segments":[{"id":0,"no_speech_prob":"high"}]}"#,
        );
        let err = client(srv.url())
            .transcribe(b"RIFF", &SttLanguage::Auto { candidates: vec![] }, None)
            .unwrap_err();
        assert!(
            err.starts_with("stt response no_speech_prob is not a number: "),
            "{err}"
        );

        let srv = MockServer::start(200, r#"{"text":"x","segments":"none"}"#);
        let err = client(srv.url())
            .transcribe(b"RIFF", &SttLanguage::Auto { candidates: vec![] }, None)
            .unwrap_err();
        assert!(
            err.starts_with("stt response segments is not an array: "),
            "{err}"
        );
    }
}
