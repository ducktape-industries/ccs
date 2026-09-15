use std::process::ExitCode;

use anyhow::{Result, bail};

use ccs::cli::{self, Cmd};
use ccs::env::Env;
use ccs::model::Provider;
use ccs::{cmd, login, picker};

fn main() -> ExitCode {
    let Err(error) = run() else { return ExitCode::SUCCESS };
    let detail = error.chain().map(|c| c.to_string()).collect::<Vec<_>>().join(": ");
    eprintln!("ccs: {detail}");
    ExitCode::FAILURE
}

fn run() -> Result<()> {
    let command = cli::parse(std::env::args().skip(1))?;
    match command {
        Cmd::Help => {
            print!("{}", cli::HELP);
            return Ok(());
        }
        Cmd::Version => {
            println!("ccs {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        _ => {}
    }

    let env = Env::open()?;
    let ctx = env.ctx();

    match command {
        Cmd::Claude { args } => ccs::routed::launch(env, &args),
        Cmd::Pick => cmd::pick(&ctx),
        Cmd::List { json, cached } => cmd::list(&ctx, json, cached),
        Cmd::Use { target, force } => cmd::use_account(&ctx, &target, force),
        Cmd::Add { name, current, email, console, sso, provider } => {
            let Some(provider) = select_provider(provider, "Add an account — choose provider")?
            else {
                return Ok(());
            };
            let options = login::Options { email, console, sso };
            cmd::add(&ctx, provider, name.as_deref(), current, &options)
        }
        Cmd::Pin { target, args } => cmd::pin(&ctx, target.as_deref(), &args),
        Cmd::Remove { target } => cmd::remove(&ctx, &target),
        Cmd::Status { json, cached, provider } => {
            let needs_menu = provider.is_none() && !json && picker::interactive();
            if !needs_menu {
                return cmd::status(&ctx, json, cached, provider);
            }
            let choices = [
                ("Both providers", StatusScope::All),
                ("Claude Code", StatusScope::One(Provider::Claude)),
                ("Codex", StatusScope::One(Provider::Codex)),
            ];
            let Some(scope) = picker::select("Show usage — choose provider", &choices)? else {
                return Ok(());
            };
            let provider = match scope {
                StatusScope::All => None,
                StatusScope::One(provider) => Some(provider),
            };
            cmd::status(&ctx, json, cached, provider)
        }
        Cmd::Notify { off, kinds, bypass, provider } => {
            let needs_menu = provider.is_none() && picker::interactive();
            let provider = if needs_menu {
                let Some(provider) = picker::provider("Session notices — choose provider")?
                else {
                    return Ok(());
                };
                Some(provider)
            } else {
                provider
            };
            cmd::notify(&ctx, off, &kinds, bypass, provider)
        }
        Cmd::Watch { every, high, rotate } => {
            cmd::watch(&ctx, std::time::Duration::from_secs(every), high, &rotate)
        }
        Cmd::Serve { port, rotate } => cmd::serve(&ctx, port, &rotate),
        Cmd::ServeKey { provider } => {
            // pi's existing `!ccs serve --key` command substitution stays usable.
            let selected =
                provider.or_else(|| (!picker::interactive()).then_some(Provider::Claude));
            let Some(provider) = select_provider(selected, "Gateway key — choose provider")?
            else {
                return Ok(());
            };
            cmd::serve_key(&ctx, provider)
        }
        Cmd::Help | Cmd::Version => unreachable!("answered before the wiring above"),
    }
}

#[derive(Clone, Copy)]
enum StatusScope {
    All,
    One(Provider),
}

fn select_provider(selected: Option<Provider>, action: &str) -> Result<Option<Provider>> {
    if let Some(provider) = selected {
        return Ok(Some(provider));
    }
    if !picker::interactive() {
        bail!("choose a provider with --claude or --codex when running without a terminal");
    }
    picker::provider(action)
}
