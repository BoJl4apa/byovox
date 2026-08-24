//! Language policy: keyboard layout in, STT request fields out.
//!
//! Depends on `config::LanguageConfig`. Produces the exact multipart fields the STT
//! request carries: an explicit `language=<code>`, or nothing plus an optional
//! `language_candidates` list. The literal "auto" is never put on the wire.

use std::collections::HashMap;

use anyhow::{Result, bail};

use crate::config::LanguageConfig;

/// An ISO 639-1 code, lowercase ASCII, exactly two letters.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Lang([u8; 2]);

impl Lang {
    pub fn parse(s: &str) -> Option<Lang> {
        let b = s.as_bytes();
        if b.len() == 2 && b.iter().all(|c| c.is_ascii_lowercase()) {
            Some(Lang([b[0], b[1]]))
        } else {
            None
        }
    }

    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).expect("ascii by construction")
    }
}

impl std::fmt::Display for Lang {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SttLanguage {
    Explicit(Lang),
    Auto { candidates: Vec<Lang> },
}

impl SttLanguage {
    /// Multipart fields for this choice. Auto with no candidates sends nothing.
    pub fn form_fields(&self) -> Vec<(&'static str, String)> {
        match self {
            SttLanguage::Explicit(l) => vec![("language", l.to_string())],
            SttLanguage::Auto { candidates } if candidates.is_empty() => vec![],
            SttLanguage::Auto { candidates } => vec![(
                "language_candidates",
                candidates
                    .iter()
                    .map(Lang::as_str)
                    .collect::<Vec<_>>()
                    .join(","),
            )],
        }
    }

    /// For the per-dictation log line: `auto` / `he`.
    pub fn label(&self) -> String {
        match self {
            SttLanguage::Explicit(l) => l.to_string(),
            SttLanguage::Auto { .. } => "auto".into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct LanguagePolicy {
    default: Option<Lang>, // None = auto
    candidates: Vec<Lang>,
    by_layout: HashMap<Lang, Lang>,
}

impl LanguagePolicy {
    pub fn from_config(c: &LanguageConfig) -> Result<Self> {
        let default = if c.default == "auto" {
            None
        } else {
            Some(parse_or_bail(&c.default, "language.default")?)
        };
        let candidates = c
            .candidates
            .iter()
            .map(|s| parse_or_bail(s, "language.candidates"))
            .collect::<Result<Vec<_>>>()?;
        let mut by_layout = HashMap::new();
        for (k, v) in &c.by_layout {
            by_layout.insert(
                parse_or_bail(k, "language.by_layout key")?,
                parse_or_bail(v, &format!("language.by_layout.{k}"))?,
            );
        }
        Ok(Self {
            default,
            candidates,
            by_layout,
        })
    }

    pub fn resolve(&self, layout: Option<Lang>) -> SttLanguage {
        if let Some(mapped) = layout.and_then(|l| self.by_layout.get(&l)) {
            return SttLanguage::Explicit(*mapped);
        }
        match self.default {
            Some(l) => SttLanguage::Explicit(l),
            None => SttLanguage::Auto {
                candidates: self.candidates.clone(),
            },
        }
    }
}

fn parse_or_bail(s: &str, what: &str) -> Result<Lang> {
    match Lang::parse(s) {
        Some(l) => Ok(l),
        None => bail!("{what}: `{s}` is not an ISO 639-1 code (two lowercase letters, e.g. `he`)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LanguageConfig;

    fn cfg(default: &str, candidates: &[&str], by_layout: &[(&str, &str)]) -> LanguageConfig {
        LanguageConfig {
            default: default.into(),
            candidates: candidates.iter().map(|s| s.to_string()).collect(),
            by_layout: by_layout
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn mapped_layout_is_explicit() {
        // A non-identity mapping, so returning the layout instead of the mapped language fails.
        let p = LanguagePolicy::from_config(&cfg("auto", &["en", "ru"], &[("ru", "en")])).unwrap();
        assert_eq!(
            p.resolve(Lang::parse("ru")),
            SttLanguage::Explicit(Lang::parse("en").unwrap())
        );
    }

    #[test]
    fn mapped_layout_beats_an_explicit_default() {
        let p = LanguagePolicy::from_config(&cfg("en", &[], &[("he", "he")])).unwrap();
        let he = Lang::parse("he").unwrap();
        assert_eq!(p.resolve(Some(he)), SttLanguage::Explicit(he));
        assert_eq!(
            p.resolve(None),
            SttLanguage::Explicit(Lang::parse("en").unwrap())
        );
    }

    #[test]
    fn unmapped_layout_is_auto_with_candidates() {
        let p = LanguagePolicy::from_config(&cfg("auto", &["en", "ru"], &[("he", "he")])).unwrap();
        let en = Lang::parse("en").unwrap();
        let ru = Lang::parse("ru").unwrap();
        assert_eq!(
            p.resolve(Lang::parse("ru")),
            SttLanguage::Auto {
                candidates: vec![en, ru]
            }
        );
        assert_eq!(
            p.resolve(None),
            SttLanguage::Auto {
                candidates: vec![en, ru]
            }
        );
    }

    #[test]
    fn explicit_default_when_not_auto() {
        let p = LanguagePolicy::from_config(&cfg("en", &[], &[])).unwrap();
        assert_eq!(
            p.resolve(None),
            SttLanguage::Explicit(Lang::parse("en").unwrap())
        );
    }

    #[test]
    fn form_fields_never_send_auto_literal() {
        let auto = SttLanguage::Auto {
            candidates: vec![Lang::parse("en").unwrap(), Lang::parse("ru").unwrap()],
        };
        assert_eq!(
            auto.form_fields(),
            vec![("language_candidates", "en,ru".to_string())]
        );
        let auto_empty = SttLanguage::Auto { candidates: vec![] };
        assert!(auto_empty.form_fields().is_empty());
        let he = SttLanguage::Explicit(Lang::parse("he").unwrap());
        assert_eq!(he.form_fields(), vec![("language", "he".to_string())]);
    }

    #[test]
    fn label_is_the_log_line_form() {
        let auto = SttLanguage::Auto {
            candidates: vec![Lang::parse("en").unwrap()],
        };
        assert_eq!(auto.label(), "auto");
        assert_eq!(
            SttLanguage::Explicit(Lang::parse("he").unwrap()).label(),
            "he"
        );
    }

    #[test]
    fn bad_codes_are_rejected_at_load() {
        assert!(Lang::parse("eng").is_none());
        assert!(Lang::parse("E1").is_none());
        assert!(Lang::parse("HE").is_none());
        assert!(Lang::parse("").is_none());
        assert!(Lang::parse("é").is_none());

        for bad in ["", "EN"] {
            let err = LanguagePolicy::from_config(&cfg(bad, &[], &[]))
                .unwrap_err()
                .to_string();
            assert!(err.contains("language.default"), "{err}");
        }
        assert!(LanguagePolicy::from_config(&cfg("auto", &["english"], &[])).is_err());
        assert!(LanguagePolicy::from_config(&cfg("auto", &[], &[("hebrew", "he")])).is_err());

        // "auto" is a config-level default, never a per-layout language.
        let err = LanguagePolicy::from_config(&cfg("auto", &[], &[("he", "auto")]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("language.by_layout.he"), "{err}");
        assert!(LanguagePolicy::from_config(&cfg("auto", &[], &[("he", "en")])).is_ok());
    }
}
