//! Launch Claude Code through a session-owned local router. Subagents inherit
//! the same endpoint, so routing is based on each request's model, not the parent.
use crate::{cmd, env::Env, routing::Routing, serve};
use anyhow::{Context, Result, bail};
use std::{
    net::TcpListener,
    process::Command,
    sync::{Arc, mpsc},
};

pub fn launch(env: Env, args: &[String]) -> Result<()> {
    Routing::read(env.ctx().stash.root())?;
    let keys = cmd::gateway_keys(&env.ctx())?;
    let listener = TcpListener::bind("127.0.0.1:0").context("starting Claude Code routing")?;
    let port = listener.local_addr()?.port();
    let binary = std::env::var("CCS_CLAUDE_BINARY").unwrap_or_else(|_| "claude".into());
    let mut command = command(&binary, port, &keys.claude, args);
    let (asks, inbox) = mpsc::channel();
    let listener = if std::env::var("CCS_LOG_REQUESTS").as_deref() == Ok("1") {
        serve::listen(listener, keys, asks)
    } else {
        serve::listen_quiet(listener, keys, asks)
    };
    let env = Arc::new(env);
    let desk_env = Arc::clone(&env);
    let _desk = std::thread::spawn(move || cmd::desk(&desk_env.ctx(), &[], inbox));
    let result = command.status().context("starting Claude Code");
    listener.stop();
    let status = result?;
    if !status.success() {
        bail!("Claude Code exited with {status}");
    }
    Ok(())
}

fn command(binary: &str, port: u16, key: &str, args: &[String]) -> Command {
    let mut command = Command::new(binary);
    command
        .args(args)
        .env("ANTHROPIC_BASE_URL", format!("http://127.0.0.1:{port}"))
        .env("CLAUDE_CODE_OAUTH_TOKEN", key)
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .env_remove("CLAUDE_CODE_OAUTH_REFRESH_TOKEN")
        .env_remove("CLAUDE_CODE_USE_BEDROCK")
        .env_remove("CLAUDE_CODE_USE_VERTEX")
        .env_remove("CLAUDE_CODE_USE_FOUNDRY");
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn launch_preserves_model_and_subagent_arguments_and_overrides_auth() {
        let args = vec!["--model".into(), "fable".into(), "--continue".into()];
        let cmd = command("claude", 4321, "local-test-key", &args);
        assert_eq!(cmd.get_args().collect::<Vec<_>>(), ["--model", "fable", "--continue"]);
        let env: std::collections::HashMap<_, _> = cmd.get_envs().collect();
        assert_eq!(
            env[std::ffi::OsStr::new("ANTHROPIC_BASE_URL")].unwrap(),
            "http://127.0.0.1:4321"
        );
        assert_eq!(env[std::ffi::OsStr::new("CLAUDE_CODE_OAUTH_TOKEN")].unwrap(), "local-test-key");
        assert!(env[std::ffi::OsStr::new("ANTHROPIC_API_KEY")].is_none());
    }
}
