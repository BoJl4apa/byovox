//! Speech-to-text client: one multipart POST to `{base_url}/audio/transcriptions`.
//!
//! Depends on `multipart` and `lang::SttLanguage`. Produces the trimmed transcript.
//! Errors are strings the pipeline logs and shows; HTTP errors are never retried,
//! transport errors once.

use std::time::Duration;

use crate::lang::SttLanguage;
use crate::multipart::Multipart;

pub trait Transcriber: Send {
    fn transcribe(
        &self,
        wav: &[u8],
        language: &SttLanguage,
        prompt: Option<&str>,
    ) -> Result<String, String>;
}

pub struct SttClient {
    url: String,
    model: String,
    api_key: Option<String>,
    agent: ureq::Agent,
}

impl SttClient {
    pub fn new(
        base_url: &str,
        model: &str,
        api_key: Option<String>,
        timeout: Duration,
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
            agent: config.into(),
        }
    }

    fn post_once(&self, content_type: &str, body: &[u8]) -> Result<(u16, String), String> {
        let mut req = self
            .agent
            .post(&self.url)
            .header("Content-Type", content_type);
        if let Some(k) = &self.api_key {
            req = req.header("Authorization", &format!("Bearer {k}"));
        }
        let mut resp = req.send(body).map_err(|e| format!("transport: {e}"))?;
        let status = resp.status().as_u16();
        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| format!("reading body: {e}"))?;
        Ok((status, text))
    }
}

impl Transcriber for SttClient {
    fn transcribe(
        &self,
        wav: &[u8],
        language: &SttLanguage,
        prompt: Option<&str>,
    ) -> Result<String, String> {
        let mut m = Multipart::new();
        m.file("file", "clip.wav", "audio/wav", wav);
        m.text("model", &self.model);
        m.text("response_format", "json");
        for (name, value) in language.form_fields() {
            m.text(name, &value);
        }
        if let Some(p) = prompt.filter(|p| !p.trim().is_empty()) {
            m.text("prompt", p);
        }
        let (content_type, body) = m.finish();

        // One retry on transport errors only.
        let (status, text) = match self.post_once(&content_type, &body) {
            Ok(r) => r,
            Err(first) => {
                tracing::warn!(error = %first, "stt transport error, retrying once");
                self.post_once(&content_type, &body)?
            }
        };
        if !(200..300).contains(&status) {
            return Err(format!(
                "stt HTTP {status}: {}",
                text.chars().take(200).collect::<String>()
            ));
        }
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("stt JSON: {e}"))?;
        Ok(v.get("text")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .trim()
            .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::{Lang, SttLanguage};
    use crate::testutil::MockServer;
    use std::time::Duration;

    fn client(url: &str) -> SttClient {
        SttClient::new(url, "whisper-1", None, Duration::from_secs(5))
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
        assert_eq!(out, "hello world");
        let req = srv.requests().remove(0);
        assert!(req.starts_with("POST /v1/audio/transcriptions "), "{req}");
        assert!(req.contains("name=\"language\"\r\n\r\nhe\r\n"));
        assert!(req.contains("name=\"prompt\"\r\n\r\nGlossary: Acme\r\n"));
        assert!(req.contains("name=\"response_format\"\r\n\r\njson\r\n"));
        assert!(req.contains("filename=\"clip.wav\""));
        assert_eq!(auth_header(&req), None, "{req}");
    }

    #[test]
    fn auto_sends_candidates_and_no_language_field() {
        let srv = MockServer::start(200, r#"{"text":"x"}"#);
        let auto = SttLanguage::Auto {
            candidates: vec![Lang::parse("en").unwrap(), Lang::parse("ru").unwrap()],
        };
        client(srv.url()).transcribe(b"RIFF", &auto, None).unwrap();
        let req = srv.requests().remove(0);
        assert!(req.contains("name=\"language_candidates\"\r\n\r\nen,ru\r\n"));
        assert!(!req.contains("name=\"language\"\r\n"));
        assert!(!req.contains("name=\"prompt\""));
    }

    #[test]
    fn bearer_header_when_key_given() {
        let srv = MockServer::start(200, r#"{"text":"x"}"#);
        let c = SttClient::new(srv.url(), "m", Some("tok".into()), Duration::from_secs(5));
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

    #[test]
    fn connection_refused_is_an_error() {
        let c = client("http://127.0.0.1:9");
        assert!(
            c.transcribe(b"RIFF", &SttLanguage::Auto { candidates: vec![] }, None)
                .is_err()
        );
    }
}
