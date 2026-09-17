//! Compiled session adapters. Messenger storage, routing and subscriptions use this boundary.
mod claude;
mod codex;
pub use claude::ClaudeCode;
pub use codex::Codex;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// An adapter submits a message to an existing session; success is not acknowledgement.
/// Implementations must bound transport waits and must never retry ambiguous submissions.
pub trait Adapter {
    fn provider(&self) -> &'static str;
    fn validate(&self) -> Result<()>;
    fn deliver(&self, body: &str) -> Result<()>;
}

/// Persisted endpoint configuration. Tags and fields are stable across server restarts.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum Session {
    Claude(ClaudeCode),
    Codex(Codex),
}

impl Session {
    pub fn adapter(&self) -> &dyn Adapter {
        match self {
            Self::Claude(adapter) => adapter,
            Self::Codex(adapter) => adapter,
        }
    }

    pub fn provider(&self) -> &'static str {
        self.adapter().provider()
    }
    pub fn validate(&self) -> Result<()> {
        self.adapter().validate()
    }
    pub fn deliver(&self, body: &str) -> Result<()> {
        self.adapter().deliver(body)
    }

    /// Registration factory for compiled adapters. New agents need no account Provider variant.
    pub fn register(
        name: Option<&str>,
        config: &Path,
        home: &Path,
        binary: &str,
        bypass: bool,
    ) -> Result<Self> {
        match name {
            Some("claude") => ClaudeCode::detect(config, bypass).map(Self::Claude),
            Some("codex") => {
                let mut adapter = Codex::detect(home, binary, bypass)?;
                adapter.binary = crate::notify::executable(binary)?;
                Ok(Self::Codex(adapter))
            }
            None => {
                // Only built-ins auto-detect. Additional adapters use an explicit name.
                let session = Self::detect(config, home, binary, None, bypass)?;
                Self::register(Some(session.provider()), config, home, binary, bypass)
            }
            Some(name) => bail!("unknown adapter: {name}; compiled adapters: claude, codex"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Adapter, Session};
    use serde_json::json;

    #[test]
    fn existing_endpoint_json_round_trips_through_the_common_adapter() {
        for (name, value) in [
            (
                "claude",
                json!({"provider":"claude","socket":"/missing.sock","config":"/config","bypass":false}),
            ),
            (
                "codex",
                json!({"provider":"codex","thread":"thread","home":"/home","binary":"/codex"}),
            ),
        ] {
            let session: Session = serde_json::from_value(value.clone()).unwrap();
            let adapter: &dyn Adapter = session.adapter();
            assert_eq!(adapter.provider(), name);
            assert_eq!(serde_json::to_value(&session).unwrap(), value);
        }
        assert!(serde_json::from_value::<Session>(json!({"provider":"unknown"})).is_err());
    }
}
