//! Model-specific gateway accounts. Rules never change the CLI's active account.
use crate::model::Provider;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const FILE: &str = "routing.json";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Routing {
    #[serde(default)]
    pub rules: Vec<Rule>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub provider: Provider,
    /// Exact model ID, or a prefix ending in '*'. Exact matches take priority.
    pub model: String,
    /// Primary account followed by explicit fallbacks on HTTP 429.
    pub accounts: Vec<String>,
}

impl Routing {
    pub fn read(root: &Path) -> Result<Self> {
        match std::fs::read(root.join(FILE)) {
            Ok(bytes) => {
                let routing: Self =
                    serde_json::from_slice(&bytes).context("reading model routes")?;
                routing.validate()?;
                Ok(routing)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn write(&self, root: &Path) -> Result<()> {
        self.validate()?;
        crate::fsx::write_atomic(&root.join(FILE), &serde_json::to_vec_pretty(self)?, 0o600)
    }

    pub fn validate(&self) -> Result<()> {
        for (i, rule) in self.rules.iter().enumerate() {
            let prefix = rule.model.strip_suffix('*').unwrap_or(&rule.model);
            if prefix.is_empty() || prefix.contains('*') || rule.model.trim() != rule.model {
                bail!("use an exact model ID or a non-empty prefix followed by *");
            }
            if rule.accounts.is_empty() || rule.accounts.iter().any(|a| a.trim().is_empty()) {
                bail!("each model route needs an account");
            }
            if self.rules[..i].iter().any(|r| r.provider == rule.provider && r.model == rule.model)
            {
                bail!("duplicate route for {} {}", rule.provider, rule.model);
            }
        }
        Ok(())
    }

    pub fn matching(&self, provider: Provider, model: &str) -> Option<&Rule> {
        self.rules
            .iter()
            .filter(|r| r.provider == provider)
            .filter(|r| {
                r.model == model || r.model.strip_suffix('*').is_some_and(|p| model.starts_with(p))
            })
            .max_by_key(|r| (!r.model.ends_with('*'), r.model.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_then_longest_prefix_and_provider_isolation() {
        let r = Routing {
            rules: vec![
                Rule {
                    provider: Provider::Claude,
                    model: "claude-*".into(),
                    accounts: vec!["a".into()],
                },
                Rule {
                    provider: Provider::Claude,
                    model: "claude-opus-*".into(),
                    accounts: vec!["b".into()],
                },
                Rule {
                    provider: Provider::Claude,
                    model: "claude-opus-4".into(),
                    accounts: vec!["c".into()],
                },
            ],
        };
        r.validate().unwrap();
        assert_eq!(r.matching(Provider::Claude, "claude-opus-4").unwrap().accounts, ["c"]);
        assert_eq!(r.matching(Provider::Claude, "claude-opus-5").unwrap().accounts, ["b"]);
        assert!(r.matching(Provider::Codex, "claude-opus-4").is_none());
        assert!(r.matching(Provider::Claude, "fable").is_none());
    }
    #[test]
    fn rejects_ambiguous_or_empty_rules() {
        for model in ["", "*", "claude*opus", " opus"] {
            let r = Routing {
                rules: vec![Rule {
                    provider: Provider::Claude,
                    model: model.into(),
                    accounts: vec!["a".into()],
                }],
            };
            assert!(r.validate().is_err());
        }
    }
}
