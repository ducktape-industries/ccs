//! Codex session transport.
use super::Adapter;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Codex {
    pub thread: String,
    pub home: PathBuf,
    pub binary: PathBuf,
}
impl Adapter for Codex {
    fn provider(&self) -> &'static str {
        "codex"
    }
    fn validate(&self) -> Result<()> {
        let Self { thread, home, binary } = self;

        anyhow::ensure!(!thread.trim().is_empty() && thread.len() <= 256, "invalid Codex thread");
        anyhow::ensure!(home.is_absolute() && home.is_dir(), "Codex home directory is missing");
        anyhow::ensure!(
            binary.is_absolute() && binary.is_file(),
            "Codex executable must be an existing absolute path"
        );
        Ok(())
    }
    fn deliver(&self, body: &str) -> Result<()> {
        let Self { thread, home, binary } = self;

        let mut child = Command::new(binary)
            .args(["queue", "--thread", thread, "--message", body])
            .env("CODEX_HOME", home)
            .current_dir(home)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("starting Codex queue")?;
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = child.try_wait()? {
                anyhow::ensure!(status.success(), "Codex queue failed ({status})");
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!("Codex queue timed out; delivery is unknown; do not blindly resend");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Codex {
    pub fn detect(home: &Path, binary: &str, bypass: bool) -> Result<Self> {
        if bypass {
            bail!(
                "--bypass attests Claude's messaging mode; Codex queue uses the session's own permissions"
            );
        }
        Ok(Self {
            thread: std::env::var("CODEX_THREAD_ID")
                .ok()
                .filter(|s| !s.is_empty())
                .context("CODEX_THREAD_ID is not set; run this from inside a Codex session")?,
            home: std::fs::canonicalize(home).context("resolving CODEX_HOME")?,
            binary: PathBuf::from(binary),
        })
    }
}
