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

/// What one Ollama host has that byovox can use, and what it has that only looks usable.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Models {
    /// Everything that can answer `/v1/chat/completions`: the polish stage, and the only
    /// stage Ollama serves.
    pub text: Vec<String>,
    /// Models whose name says whisper. Ollama cannot run one: a whisper GGUF is built for
    /// whisper.cpp, and Ollama's llama.cpp runner refuses it with `unknown model
    /// architecture` — measured against `sendmeaiohyeah/whisper-large-v2`, whose only
    /// advertised capability is `completion`. `/v1/audio/transcriptions` exists on the
    /// OpenAI surface and answers 500 when it tries to load one.
    ///
    /// Kept, rather than dropped, so the wizard can say that out loud. A user who pulled one
    /// did it expecting exactly this to work, and silence would leave them reading a 500.
    pub unusable_speech: Vec<String>,
}

impl Models {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.unusable_speech.is_empty()
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

/// Sort a flat list of model names into what the polish stage can use and what only looks as
/// though it could, deduplicated so the numbered menu the wizard prints is stable.
pub fn classify(names: &[String]) -> Models {
    let mut m = Models::default();
    for name in names {
        if is_speech(name) {
            m.unusable_speech.push(name.clone());
        } else if !is_embedding(name) {
            m.text.push(name.clone());
        }
    }
    for list in [&mut m.text, &mut m.unusable_speech] {
        list.sort();
        list.dedup();
    }
    m
}

/// The model names in an `/api/tags` body. Anything that is not the shape Ollama documents
/// yields nothing rather than an error: this is a convenience probe, and the wizard's next
/// move when it finds nothing is to ask the question by hand.
///
/// Cloud models served by Ollama (e.g. `gemma4:31b-cloud`) are included — users can opt in if
/// they understand the API call routes through `https://ollama.com`. The wizard may warn about
/// this when offering such a model.
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
        assert_eq!(m.text, ["llama3.2:latest", "qwen2.5:7b"]);
        assert_eq!(
            m.unusable_speech,
            ["ZimaBlueAI/whisper-large-v3:latest"],
            "named so the wizard can say why, never offered"
        );
    }

    #[test]
    fn a_body_that_is_not_a_catalogue_yields_nothing() {
        assert!(names_in("not json").is_empty());
        assert!(names_in(r#"{"models":null}"#).is_empty());
        assert!(classify(&names_in("{}")).is_empty());
    }

    /// A real `/api/tags`, trimmed to the keys that decide anything. `gemma4:31b-cloud` is
    /// the case this exists for: Ollama lists it beside the local models and answers for it
    /// over the same OpenAI surface, but serves it from `ollama.com` — so offering it would
    /// send every dictation off the machine under the heading "models you already have".
    #[test]
    fn cloud_models_are_included_in_the_catalogue() {
        let body = r#"{"models":[
            {"name":"sendmeaiohyeah/whisper-large-v2:latest","model":"sendmeaiohyeah/whisper-large-v2:latest"},
            {"name":"gemma4:e2b","model":"gemma4:e2b"},
            {"name":"gemma4:31b-cloud","model":"gemma4:31b","remote_host":"https://ollama.com"}
        ]}"#;
        let m = classify(&names_in(body));
        assert_eq!(
            m.unusable_speech,
            ["sendmeaiohyeah/whisper-large-v2:latest"]
        );
        assert_eq!(m.text, ["gemma4:31b-cloud", "gemma4:e2b"], "cloud models are offered alongside local ones");
    }

    #[test]
    fn the_openai_surface_is_the_host_plus_v1() {
        assert_eq!(
            base_url("http://127.0.0.1:11434"),
            "http://127.0.0.1:11434/v1"
        );
        assert_eq!(base_url("http://h:11434/"), "http://h:11434/v1");
    }
}
