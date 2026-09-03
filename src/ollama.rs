//! Ollama discovery: what is installed on this machine, asked of the daemon itself.
//!
//! Ollama serves an OpenAI-compatible surface under `/v1`, so once a model is picked the rest
//! of byovox needs nothing special — `[stt]` and `[polish]` point at it like any other
//! endpoint. What is not OpenAI-compatible is the *catalogue*: `/api/tags` is Ollama's own,
//! and it is the only way to answer "which models does this user already have?" without
//! making them type a name they have to look up first.

use std::time::Duration;

/// Where Ollama listens unless its user moved it.
pub const DEFAULT_HOST: &str = "http://127.0.0.1:11434";

/// The OpenAI-compatible base URL of an Ollama host: what goes in `base_url`.
pub fn base_url(host: &str) -> String {
    format!("{}/v1", host.trim_end_matches('/'))
}

/// Long enough for a busy loopback daemon, short enough that a host which is not there does
/// not stall a wizard question.
const TIMEOUT: Duration = Duration::from_secs(2);

/// What one Ollama host has, split the way the two stages need it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Models {
    /// Speech models — a name carrying `whisper` and nothing else does.
    pub stt: Vec<String>,
    /// Everything that can answer `/v1/chat/completions`: neither a speech model nor an
    /// embedding one.
    pub text: Vec<String>,
}

impl Models {
    pub fn is_empty(&self) -> bool {
        self.stt.is_empty() && self.text.is_empty()
    }
}

/// Whether a model name names a speech-to-text model. Registry names vary wildly
/// (`ZimaBlueAI/whisper-large-v3`, `dimavz/whisper-tiny:latest`), and the one thing every
/// one of them carries is the word.
fn is_speech(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("whisper") || n.contains("faster-distil") || n.contains("speech-to-text")
}

/// Whether a model name names an embedding model, which can serve neither stage.
fn is_embedding(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("embed") || n.contains("bge-") || n.contains("minilm")
}

/// Split a flat list of model names into the two stages' candidates, sorted and deduplicated
/// so the numbered menu the wizard prints is stable between runs.
pub fn classify(names: &[String]) -> Models {
    let mut m = Models::default();
    for name in names {
        if is_speech(name) {
            m.stt.push(name.clone());
        } else if !is_embedding(name) {
            m.text.push(name.clone());
        }
    }
    for list in [&mut m.stt, &mut m.text] {
        list.sort();
        list.dedup();
    }
    m
}

/// The model names in an `/api/tags` body. Anything that is not the shape Ollama documents
/// yields nothing rather than an error: this is a convenience probe, and the wizard's next
/// move when it finds nothing is to ask the question by hand.
fn names_in(body: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    v.get("models")
        .and_then(|m| m.as_array())
        .map(|models| {
            models
                .iter()
                .filter_map(|m| m.get("name").or_else(|| m.get("model")))
                .filter_map(|n| n.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// What `host` has installed, or `None` when nothing answered there. Never an error: a
/// missing Ollama is the ordinary case, not a fault.
pub fn discover(host: &str) -> Option<Models> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(TIMEOUT))
        .timeout_global(Some(TIMEOUT))
        .http_status_as_error(false)
        .build()
        .into();
    let url = format!("{}/api/tags", host.trim_end_matches('/'));
    let mut resp = agent.get(&url).call().ok()?;
    if !(200..300).contains(&resp.status().as_u16()) {
        return None;
    }
    let body = resp.body_mut().read_to_string().ok()?;
    Some(classify(&names_in(&body)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_are_split_by_what_each_model_can_serve() {
        let body = r#"{"models":[
            {"name":"llama3.2:latest"},
            {"name":"ZimaBlueAI/whisper-large-v3:latest"},
            {"name":"nomic-embed-text:latest"},
            {"name":"qwen2.5:7b"}
        ]}"#;
        let m = classify(&names_in(body));
        assert_eq!(m.stt, ["ZimaBlueAI/whisper-large-v3:latest"]);
        assert_eq!(m.text, ["llama3.2:latest", "qwen2.5:7b"]);
    }

    #[test]
    fn a_body_that_is_not_a_catalogue_yields_nothing() {
        assert!(names_in("not json").is_empty());
        assert!(names_in(r#"{"models":null}"#).is_empty());
        assert!(classify(&names_in("{}")).is_empty());
    }

    #[test]
    fn the_openai_surface_is_the_host_plus_v1() {
        assert_eq!(base_url("http://127.0.0.1:11434"), "http://127.0.0.1:11434/v1");
        assert_eq!(base_url("http://h:11434/"), "http://h:11434/v1");
    }
}
