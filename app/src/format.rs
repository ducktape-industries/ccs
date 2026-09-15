//! The `pure` side of the boundary: text the view and the menu bar compose
//! from the accounts. Deterministic, effect-free, and tested here rather than
//! through a window.

use crate::backend::Account;

/// What the menu bar shows: the active Claude account's session, since that
/// is the window that runs out within a working day.
pub fn bar_label(accounts: &[Account]) -> String {
    accounts
        .iter()
        .find(|a| a.active && a.provider == "claude" && a.session_percent >= 0.0)
        .map(|a| percent_label(a.session_percent))
        .unwrap_or_else(|| "–".to_string())
}

/// What the confirmation asks before switching into a spent account.
pub fn confirm_question(accounts: &[Account], slug: &str) -> String {
    match accounts.iter().find(|a| a.slug == slug) {
        Some(account) => {
            format!("{} has nothing left on one of its limits. Switch anyway?", account.email)
        }
        None => String::new(),
    }
}

/// Whether `port` names a port a listener can take.
pub fn is_port(port: &str) -> bool {
    port.trim().parse::<u16>().is_ok_and(|p| p > 0)
}

/// The pool with `slug` added or taken out.
pub fn toggled(pool: &[String], slug: String, on: bool) -> Vec<String> {
    let mut next: Vec<String> = pool.iter().filter(|s| **s != slug).cloned().collect();
    if on {
        next.push(slug);
    }
    next
}

/// The pool the daemons are handed: the one ticked, only while rotation is on.
pub fn pool_for(rotation_on: bool, pool: &[String]) -> Vec<String> {
    match rotation_on {
        true => pool.to_vec(),
        false => Vec::new(),
    }
}

/// When the watcher last looked: the newest reading's clock.
pub fn polled_line(accounts: &[Account]) -> String {
    let newest = accounts
        .iter()
        .filter(|a| !a.polled_at.is_empty())
        .max_by(|a, b| a.polled_at.cmp(&b.polled_at));
    match newest {
        Some(account) => format!("polled {}", account.polled),
        None => "not polled yet".to_string(),
    }
}

/// What the watcher's last turn came to, in a line under its switch.
pub fn watcher_said(notices: &[String], rotated: &[String]) -> String {
    let mut lines: Vec<&str> = notices.iter().map(String::as_str).collect();
    lines.extend(rotated.iter().map(String::as_str));
    match lines.is_empty() {
        true => "polled; nothing to report".to_string(),
        false => lines.join(" · "),
    }
}

/// A percentage as the table prints it; a dash for a window not reported.
pub fn percent_label(percent: f64) -> String {
    match percent < 0.0 {
        true => "–".to_string(),
        false => format!("{}%", percent.round() as i64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{Account, Limit};

    fn limit(column: &str, percent: f64) -> Limit {
        Limit { column: column.into(), percent, resets_in: "3h04m".into(), health: "ok".into() }
    }

    fn account(provider: &str, slug: &str, active: bool, session: f64) -> Account {
        Account {
            provider: provider.into(),
            slug: slug.into(),
            email: format!("{slug}@x.com"),
            plan: if provider == "codex" { "codex pro".into() } else { "max20x".into() },
            active,
            spent: false,
            session_percent: session,
            polled_at: "2026-09-07T00:00:00Z".into(),
            polled: "09:00".into(),
            note: String::new(),
            limits: vec![limit("session", session), limit("weekly", 37.0), limit("Fable", 62.0)],
        }
    }

    #[test]
    fn the_bar_shows_the_active_claude_accounts_session() {
        let accounts = vec![
            account("codex", "g", true, -1.0),
            account("claude", "a", false, 5.0),
            account("claude", "h", true, 52.4),
        ];
        assert_eq!(bar_label(&accounts), "52%");
    }

    #[test]
    fn the_bar_shows_a_dash_when_nothing_is_known() {
        assert_eq!(bar_label(&[]), "–");
        assert_eq!(bar_label(&[account("codex", "g", true, -1.0)]), "–");
    }

    #[test]
    fn the_question_names_the_account_and_nobody_when_there_is_none() {
        let accounts = vec![account("claude", "r", false, 100.0)];
        assert_eq!(
            confirm_question(&accounts, "r"),
            "r@x.com has nothing left on one of its limits. Switch anyway?"
        );
        assert_eq!(confirm_question(&accounts, "x"), "");
    }

    #[test]
    fn the_pool_is_ticked_and_unticked_by_slug() {
        let pool = vec!["a".to_string()];
        assert_eq!(toggled(&pool, "b".into(), true), ["a", "b"]);
        assert_eq!(toggled(&pool, "a".into(), false), Vec::<String>::new());
        assert_eq!(toggled(&pool, "a".into(), true), ["a"]);
        assert_eq!(pool_for(false, &pool), Vec::<String>::new());
        assert_eq!(pool_for(true, &pool), ["a"]);
    }

    #[test]
    fn the_polled_line_and_a_port_read_as_expected() {
        assert_eq!(polled_line(&[account("claude", "a", true, 5.0)]), "polled 09:00");
        assert_eq!(polled_line(&[]), "not polled yet");
        assert!(is_port("4141") && !is_port("0") && !is_port("70000") && !is_port("x"));
    }

    #[test]
    fn the_watcher_line_is_what_the_turn_did_or_that_it_did_nothing() {
        assert_eq!(watcher_said(&[], &[]), "polled; nothing to report");
        assert_eq!(
            watcher_said(&["session-high: a".into()], &["switched to b".into()]),
            "session-high: a · switched to b"
        );
    }

    #[test]
    fn a_percent_is_whole_or_unknown() {
        assert_eq!(percent_label(52.4), "52%");
        assert_eq!(percent_label(-1.0), "–");
    }
}
