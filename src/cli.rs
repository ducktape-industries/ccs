//! Argument parsing.

use anyhow::{Result, bail};

use crate::model::Provider;

pub const HELP: &str = "\
ccs - Claude Code and Codex account switcher

USAGE
    ccs                      pick an account interactively
    ccs claude [-- <args>]   run Claude Code with per-request model account routing
    ccs routes               choose a model and its primary/fallback accounts
    ccs ls                   every stashed account and what it has left
    ccs use <account>        switch that provider's login to an account
    ccs pin [claude|codex] [<account>]
                             start a session confined to one account, leaving
                             every other session on the account in use
    ccs add                  choose Claude Code or Codex, then log in and stash
                             another account without disturbing the one in use
    ccs add --current        choose a provider and stash its current login
    ccs rm <account>         forget a stashed account
    ccs repair               restore Claude stash slots from UUID-verified pens
    ccs status               choose a provider (or both) and show current usage
    ccs notify [<kind>...]   from inside Claude Code or Codex: have notices
                             delivered into that session's chat. Kinds:
                             switch, session-high, session-reset, weekly-reset;
                             all of them when none is named. `ccs notify off`
                             stops them. A Claude session with permission
                             prompts bypassed adds --bypass, or it will hold
                             every notice for review. Codex uses `codex queue`;
                             choose the provider in a terminal; otherwise the
                             calling session determines it.
    ccs watch                poll every account and raise session-high,
                             session-reset and weekly-reset notices; runs
                             until killed. With --rotate, also switch away
                             from a pooled account whose session has run
                             high, to the pooled account whose weekly window
                             resets soonest and still has room
    ccs serve                serve the Anthropic API on 127.0.0.1 as the account
                             in use, for pi and anything else that speaks it;
                             runs until killed. Prints the models.json snippet
                             to paste into pi. With --rotate, a request the
                             account in use is too limited to answer is sent
                             again as the next pooled account
    ccs serve --key          choose whose gateway key to print
    ccs server               local session messenger (separate from API gateway)
    ccs mcp                  compact messaging tool over MCP stdio
    ccs session --help       registration, labels, queue, inbox and replies

    <account> is a slug, an email, an unambiguous prefix of either, or the
    index shown by `ccs ls`. Without one, `ccs pin` asks. Name a provider
    before the account to resolve an email shared by Claude and Codex.

    Provider menus require a terminal. Scripts adding accounts must select
    --claude or --codex. JSON/piped status shows both by default; piped
    serve --key keeps its Claude default (or name codex/claude after --key).

    Anything after `--` is passed on to the selected client: `ccs pin -- --continue`.

OPTIONS
    -f, --force              switch even to an account with no headroom left
        --json               machine-readable output (ls, status)
        --cached             what the last poll wrote down, without polling (ls, status);
                             `ccs watch` is what keeps that current
        --name <slug>        stash under this name instead of the email (add)
        --email <address>    pre-fill the login page (add)
        --console            log in with Console billing, not a subscription (add)
        --sso                force the SSO login flow (add)
        --codex              choose Codex without a menu (add, status, notify, routes, serve --key)
        --claude             choose Claude without a menu (same commands)
        --every <seconds>    poll interval (watch; default 300)
        --high <percent>     session percentage that counts as high (watch; default 90)
        --rotate <a>,<b>,... accounts to rotate between (watch) or fall over
                             to (serve); repeatable
        --port <n>           port to serve on (serve; default 4141)
    -h, --help               this text
    -V, --version            version
";

#[derive(Debug, Clone)]
pub enum Cmd {
    Messenger {
        args: Vec<String>,
    },
    Claude {
        args: Vec<String>,
    },
    Pick,
    Routes {
        provider: Option<Provider>,
    },
    List {
        json: bool,
        cached: bool,
    },
    Use {
        target: String,
        force: bool,
    },
    Add {
        name: Option<String>,
        current: bool,
        email: Option<String>,
        console: bool,
        sso: bool,
        provider: Option<Provider>,
    },
    Pin {
        provider: Option<Provider>,
        target: Option<String>,
        args: Vec<String>,
    },
    Remove {
        target: String,
    },
    Repair,
    Status {
        json: bool,
        cached: bool,
        provider: Option<Provider>,
    },
    Notify {
        off: bool,
        kinds: Vec<String>,
        bypass: bool,
        provider: Option<Provider>,
    },
    Watch {
        every: u64,
        high: f64,
        rotate: Vec<String>,
    },
    Serve {
        port: u16,
        rotate: Vec<String>,
    },
    ServeKey {
        provider: Option<Provider>,
    },
    Help,
    Version,
}

fn selected_provider(args: &[String]) -> Result<Option<Provider>> {
    match (has(args, "--claude"), has(args, "--codex")) {
        (true, true) => bail!("choose one provider: --claude or --codex"),
        (true, false) => Ok(Some(Provider::Claude)),
        (false, true) => Ok(Some(Provider::Codex)),
        (false, false) => Ok(None),
    }
}

pub fn parse<I: Iterator<Item = String>>(args: I) -> Result<Cmd> {
    let args: Vec<String> = args.collect();
    let Some(head) = args.first() else { return Ok(Cmd::Pick) };

    match head.as_str() {
        "mcp" | "server" | "session" | "sessions" | "queue" | "inbox" | "reply" | "message" => {
            Ok(Cmd::Messenger { args })
        }
        "-h" | "--help" | "help" => Ok(Cmd::Help),
        "-V" | "--version" | "version" => Ok(Cmd::Version),
        "claude" => Ok(Cmd::Claude {
            args: args[1..].strip_prefix(&["--".to_string()]).unwrap_or(&args[1..]).to_vec(),
        }),
        "routes" => {
            if args[1..].iter().any(|a| a != "--claude" && a != "--codex") {
                bail!("usage: ccs routes [--claude | --codex]");
            }
            Ok(Cmd::Routes { provider: selected_provider(&args[1..])? })
        }
        "ls" | "list" => {
            Ok(Cmd::List { json: has(&args[1..], "--json"), cached: has(&args[1..], "--cached") })
        }
        "status" | "st" => Ok(Cmd::Status {
            json: has(&args[1..], "--json"),
            cached: has(&args[1..], "--cached"),
            provider: selected_provider(&args[1..])?,
        }),
        "notify" => {
            let rest = &args[1..];
            Ok(Cmd::Notify {
                off: has(rest, "off"),
                kinds: rest
                    .iter()
                    .filter(|a| *a != "off" && !a.starts_with("--"))
                    .cloned()
                    .collect(),
                bypass: has(rest, "--bypass"),
                provider: selected_provider(rest)?,
            })
        }
        "watch" => {
            let rest = &args[1..];
            let number = |flag: &str, fallback: f64| -> Result<f64> {
                match value(rest, flag) {
                    None => Ok(fallback),
                    Some(v) => {
                        v.parse().map_err(|_| anyhow::anyhow!("{flag} wants a number, not {v:?}"))
                    }
                }
            };
            Ok(Cmd::Watch {
                every: number("--every", 300.0)? as u64,
                high: number("--high", 90.0)?,
                rotate: pool(rest),
            })
        }
        "serve" | "gateway" => {
            let rest = &args[1..];
            if has(rest, "--key") {
                let named = match value(rest, "--key") {
                    Some(named) if !named.starts_with('-') => {
                        Some(named.parse().map_err(anyhow::Error::msg)?)
                    }
                    _ => None,
                };
                let flagged = selected_provider(rest)?;
                let conflicting = matches!((named, flagged), (Some(a), Some(b)) if a != b);
                if conflicting {
                    bail!("choose one provider for --key");
                }
                let provider = named.or(flagged);
                return Ok(Cmd::ServeKey { provider });
            }
            let port = match value(rest, "--port") {
                None => 4141,
                Some(v) => match v.parse() {
                    Ok(0) | Err(_) => bail!("--port wants a port, not {v:?}"),
                    Ok(port) => port,
                },
            };
            Ok(Cmd::Serve { port, rotate: pool(rest) })
        }
        "use" | "switch" => {
            let rest = &args[1..];
            let Some(target) = positional(rest) else {
                bail!("`ccs use` needs an account; `ccs ls` lists them");
            };
            Ok(Cmd::Use { target, force: has(rest, "--force") || has(rest, "-f") })
        }
        "add" | "capture" => {
            let rest = &args[1..];
            let provider = selected_provider(rest)?;
            if provider == Some(Provider::Codex) {
                for flag in ["--email", "--console", "--sso"] {
                    if has(rest, flag) {
                        bail!("{flag} steers Claude's login page; a Codex login has none");
                    }
                }
            }
            Ok(Cmd::Add {
                name: value(rest, "--name"),
                current: has(rest, "--current"),
                email: value(rest, "--email"),
                console: has(rest, "--console"),
                sso: has(rest, "--sso"),
                provider,
            })
        }
        "pin" | "confine" => {
            let (mine, forwarded) = forwarded(&args[1..]);
            if mine.iter().any(|arg| arg.starts_with('-')) {
                bail!("usage: ccs pin [claude|codex] [<account>] [-- <client args>]");
            }
            let provider = match mine.first().map(String::as_str) {
                Some("claude") => Some(Provider::Claude),
                Some("codex") => Some(Provider::Codex),
                _ => None,
            };
            let target = mine.get(usize::from(provider.is_some())).cloned();
            if mine.len() > 1 + usize::from(provider.is_some()) {
                bail!("usage: ccs pin [claude|codex] [<account>] [-- <client args>]");
            }
            Ok(Cmd::Pin { provider, target, args: forwarded })
        }
        "rm" | "remove" | "forget" => {
            let Some(target) = positional(&args[1..]) else {
                bail!("`ccs rm` needs an account; `ccs ls` lists them");
            };
            Ok(Cmd::Remove { target })
        }
        "repair" => {
            if args.len() != 1 {
                bail!("usage: ccs repair");
            }
            Ok(Cmd::Repair)
        }
        other => bail!("unknown command {other:?}; `ccs --help` lists them"),
    }
}

/// Every account named after a `--rotate`, comma-separated or repeated.
fn pool(args: &[String]) -> Vec<String> {
    values(args, "--rotate")
        .flat_map(|v| v.split(',').map(str::trim).map(String::from).collect::<Vec<_>>())
        .filter(|v| !v.is_empty())
        .collect()
}

/// Split at `--`: what follows belongs to the command being launched rather
/// than to this one, flags and all.
fn forwarded(args: &[String]) -> (&[String], Vec<String>) {
    let Some(at) = args.iter().position(|a| a == "--") else { return (args, Vec::new()) };
    (&args[..at], args[at + 1..].to_vec())
}

fn has(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

/// The value following `flag`, when present.
fn value(args: &[String], flag: &str) -> Option<String> {
    let at = args.iter().position(|a| a == flag)?;
    args.get(at + 1).cloned()
}

/// Every value following an occurrence of `flag`.
fn values<'a>(args: &'a [String], flag: &'a str) -> impl Iterator<Item = &'a String> + 'a {
    args.windows(2).filter(move |w| w[0] == flag).map(|w| &w[1])
}

/// Flags that consume the argument after them.
const VALUE_FLAGS: [&str; 6] = ["--name", "--email", "--every", "--high", "--rotate", "--port"];

/// The first argument that is neither a flag nor a flag's value.
fn positional(args: &[String]) -> Option<String> {
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if VALUE_FLAGS.contains(&arg.as_str()) {
            skip_next = true;
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        return Some(arg.clone());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(words: &[&str]) -> Cmd {
        parse(words.iter().map(|w| w.to_string())).expect("parses")
    }

    #[test]
    fn status_selects_a_provider_and_can_read_without_polling() {
        assert!(matches!(
            parsed(&["status", "--codex", "--cached", "--json"]),
            Cmd::Status { json: true, cached: true, provider: Some(Provider::Codex) }
        ));
        assert!(matches!(parsed(&["status"]), Cmd::Status { cached: false, provider: None, .. }));
        assert!(parse(["status", "--claude", "--codex"].map(String::from).into_iter()).is_err());
    }

    #[test]
    fn notify_can_select_codex_without_treating_the_flag_as_a_kind() {
        let Cmd::Notify { off, kinds, provider, .. } =
            parsed(&["notify", "--codex", "session-high"])
        else {
            panic!("notify")
        };
        assert!(!off);
        assert_eq!(kinds, ["session-high"]);
        assert_eq!(provider, Some(Provider::Codex));
        assert!(matches!(
            parsed(&["notify", "off", "--codex"]),
            Cmd::Notify { off: true, provider: Some(Provider::Codex), .. }
        ));
    }

    #[test]
    fn no_arguments_opens_the_picker() {
        assert!(matches!(parsed(&[]), Cmd::Pick));
    }

    #[test]
    fn routes_selects_a_provider_and_rejects_ambiguous_or_unknown_flags() {
        assert!(matches!(parsed(&["routes"]), Cmd::Routes { provider: None }));
        assert!(matches!(
            parsed(&["routes", "--codex"]),
            Cmd::Routes { provider: Some(Provider::Codex) }
        ));
        assert!(matches!(
            parsed(&["routes", "--claude"]),
            Cmd::Routes { provider: Some(Provider::Claude) }
        ));
        for args in [vec!["routes", "--codex", "--claude"], vec!["routes", "--json"]] {
            assert!(parse(args.into_iter().map(String::from)).is_err());
        }
    }

    #[test]
    fn list_takes_an_optional_json_flag() {
        assert!(matches!(parsed(&["ls"]), Cmd::List { json: false, cached: false }));
        assert!(matches!(parsed(&["list", "--json"]), Cmd::List { json: true, .. }));
    }

    #[test]
    fn list_can_be_asked_for_the_last_readings_rather_than_a_poll() {
        assert!(matches!(
            parsed(&["ls", "--cached", "--json"]),
            Cmd::List { json: true, cached: true }
        ));
    }

    #[test]
    fn use_takes_a_target_and_an_optional_force() {
        let Cmd::Use { target, force } = parsed(&["use", "work"]) else { panic!("not a use") };
        assert_eq!((target.as_str(), force), ("work", false));

        let Cmd::Use { force, .. } = parsed(&["use", "work", "-f"]) else { panic!("not a use") };
        assert!(force);
    }

    #[test]
    fn a_flag_before_the_target_does_not_become_the_target() {
        let Cmd::Use { target, force } = parsed(&["use", "--force", "work"]) else {
            panic!("not a use")
        };
        assert_eq!((target.as_str(), force), ("work", true));
    }

    #[test]
    fn add_logs_in_by_default_and_captures_the_live_account_only_on_request() {
        assert!(matches!(parsed(&["add"]), Cmd::Add { current: false, .. }));
        assert!(matches!(parsed(&["add", "--current"]), Cmd::Add { current: true, .. }));
    }

    #[test]
    fn add_reads_the_name_flag_without_treating_it_as_a_target() {
        let Cmd::Add { name, .. } = parsed(&["add", "--name", "work"]) else {
            panic!("not an add")
        };
        assert_eq!(name.as_deref(), Some("work"));
        assert!(matches!(parsed(&["add"]), Cmd::Add { name: None, .. }));
    }

    /// The login-page flags steer Claude's page; a Codex login has none
    /// to steer, so asking for both is a mistake said out loud.
    #[test]
    fn add_refuses_login_page_flags_with_codex() {
        let words = ["add", "--codex", "--email", "me@x.com"].map(String::from);
        assert!(parse(words.into_iter()).is_err());
        assert!(matches!(
            parsed(&["add", "--codex", "--current"]),
            Cmd::Add { provider: Some(Provider::Codex), current: true, .. }
        ));
    }

    #[test]
    fn provider_critical_commands_leave_an_omitted_provider_for_the_menu() {
        assert!(matches!(parsed(&["add"]), Cmd::Add { provider: None, .. }));
        assert!(matches!(
            parsed(&["add", "--claude"]),
            Cmd::Add { provider: Some(Provider::Claude), .. }
        ));
        assert!(matches!(
            parsed(&["serve", "--key", "--codex"]),
            Cmd::ServeKey { provider: Some(Provider::Codex) }
        ));
        for args in
            [vec!["add", "--claude", "--codex"], vec!["serve", "--key", "codex", "--claude"]]
        {
            assert!(parse(args.into_iter().map(String::from)).is_err());
        }
    }

    #[test]
    fn add_forwards_the_flags_that_steer_the_login_page() {
        let Cmd::Add { email, console, sso, .. } =
            parsed(&["add", "--email", "me@x.com", "--console", "--sso"])
        else {
            panic!("not an add")
        };
        assert_eq!(email.as_deref(), Some("me@x.com"));
        assert!(console && sso);
    }

    #[test]
    fn a_value_carrying_flag_does_not_donate_its_value_as_a_target() {
        let Cmd::Remove { target } = parsed(&["rm", "--name", "notthetarget", "work"]) else {
            panic!("not a remove")
        };
        assert_eq!(target, "work");
    }

    #[test]
    fn remove_needs_a_target() {
        let Cmd::Remove { target } = parsed(&["rm", "work"]) else { panic!("not a remove") };
        assert_eq!(target, "work");
        assert!(parse(["rm".to_string()].into_iter()).is_err());
    }

    #[test]
    fn use_without_a_target_is_an_error_not_a_silent_no_op() {
        assert!(parse(["use".to_string()].into_iter()).is_err());
    }

    #[test]
    fn pin_without_a_target_asks_rather_than_failing() {
        let Cmd::Pin { provider, target, args } = parsed(&["pin"]) else { panic!("not a pin") };
        assert_eq!(provider, None);
        assert_eq!(target, None);
        assert!(args.is_empty());
    }

    #[test]
    fn pin_takes_a_target_when_one_is_given() {
        let Cmd::Pin { target, .. } = parsed(&["pin", "work"]) else { panic!("not a pin") };
        assert_eq!(target.as_deref(), Some("work"));
    }

    #[test]
    fn pin_hands_everything_after_a_double_dash_to_claude_code() {
        let Cmd::Pin { target, args, .. } =
            parsed(&["pin", "work", "--", "--continue", "-p", "hi"])
        else {
            panic!("not a pin")
        };
        assert_eq!(target.as_deref(), Some("work"));
        assert_eq!(args, ["--continue", "-p", "hi"]);
    }

    #[test]
    fn a_forwarded_flag_is_never_mistaken_for_the_target() {
        let Cmd::Pin { target, args, .. } = parsed(&["pin", "--", "resume"]) else {
            panic!("not a pin")
        };
        assert_eq!(target, None);
        assert_eq!(args, ["resume"]);
    }

    #[test]
    fn pin_accepts_a_provider_before_an_optional_account() {
        assert!(matches!(
            parsed(&["pin", "claude", "shared@example.com"]),
            Cmd::Pin { provider: Some(Provider::Claude), target: Some(target), .. }
                if target == "shared@example.com"
        ));
        assert!(matches!(
            parsed(&["pin", "codex"]),
            Cmd::Pin { provider: Some(Provider::Codex), target: None, .. }
        ));
        assert!(matches!(
            parsed(&["pin", "codex", "shared@example.com", "--", "resume"]),
            Cmd::Pin { provider: Some(Provider::Codex), target: Some(target), args }
                if target == "shared@example.com" && args == ["resume"]
        ));
        assert!(parse(["pin", "claude", "hong", "extra"].map(String::from).into_iter()).is_err());
    }

    #[test]
    fn watch_collects_a_rotation_pool_from_commas_and_repeats() {
        let Cmd::Watch { rotate, high, .. } =
            parsed(&["watch", "--rotate", "a, b", "--high", "95", "--rotate", "c"])
        else {
            panic!("not a watch")
        };
        assert_eq!(rotate, ["a", "b", "c"]);
        assert_eq!(high, 95.0);
        assert!(matches!(parsed(&["watch"]), Cmd::Watch { rotate, .. } if rotate.is_empty()));
    }

    #[test]
    fn serve_defaults_its_port_and_takes_a_pool_like_watch() {
        let Cmd::Serve { port, rotate } = parsed(&["serve"]) else { panic!("not a serve") };
        assert_eq!((port, rotate.is_empty()), (4141, true));

        let Cmd::Serve { port, rotate } = parsed(&["serve", "--port", "8080", "--rotate", "a,b"])
        else {
            panic!("not a serve")
        };
        assert_eq!(port, 8080);
        assert_eq!(rotate, ["a", "b"]);
    }

    #[test]
    fn serve_key_only_prints_the_key_of_the_provider_named() {
        assert!(matches!(parsed(&["serve", "--key"]), Cmd::ServeKey { provider: None }));
        assert!(matches!(
            parsed(&["serve", "--key", "codex"]),
            Cmd::ServeKey { provider: Some(Provider::Codex) }
        ));
        assert!(
            parse(["serve".to_string(), "--key".to_string(), "gemini".to_string()].into_iter())
                .is_err()
        );
    }

    #[test]
    fn serve_refuses_port_zero_since_the_snippet_could_not_name_it() {
        assert!(
            parse(["serve".to_string(), "--port".to_string(), "0".to_string()].into_iter())
                .is_err()
        );
    }

    #[test]
    fn serve_refuses_a_port_that_is_not_one() {
        assert!(
            parse(["serve".to_string(), "--port".to_string(), "lots".to_string()].into_iter())
                .is_err()
        );
    }

    #[test]
    fn an_unknown_command_is_refused() {
        assert!(parse(["frobnicate".to_string()].into_iter()).is_err());
    }
}
