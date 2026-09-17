//! ClaudeCode session transport.
use super::Adapter;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClaudeCode {
    pub socket: String,
    pub config: PathBuf,
    pub bypass: bool,
}
impl Adapter for ClaudeCode {
    fn provider(&self) -> &'static str {
        "claude"
    }
    fn validate(&self) -> Result<()> {
        let Self { socket, config, .. } = self;

        anyhow::ensure!(
            Path::new(socket).is_absolute() && Path::new(socket).exists(),
            "Claude messaging socket is missing or not absolute"
        );
        anyhow::ensure!(
            config.is_absolute() && config.is_dir(),
            "Claude config directory is missing"
        );
        Ok(())
    }
    fn deliver(&self, body: &str) -> Result<()> {
        let Self { socket, config, bypass } = self;

        let mode = bypass.then_some("bypass");
        let attest = mode.map(|m| format!(" from-mode=\"{m}\"")).unwrap_or_default();
        let body = format!(
            "<cross-session-message from-name=\"ccs\"{attest}>\n{body}\n</cross-session-message>"
        );
        crate::notify::send(&config.join("sessions"), socket, &body, mode)
            .context("Claude socket submission failed or could not be confirmed")
    }
}

impl ClaudeCode {
    pub fn detect(config: &Path, bypass: bool) -> Result<Self> {
        Ok(Self {
            socket: std::env::var("CLAUDE_CODE_MESSAGING_SOCKET").ok().filter(|s| !s.is_empty())
                .context("CLAUDE_CODE_MESSAGING_SOCKET is not set; run this from inside a Claude Code session")?,
            config: std::fs::canonicalize(config)?, bypass,
        })
    }
}
