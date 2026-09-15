//! Rendering the usage table: bars, colour, and the dynamic column set.
//!
//! Columns follow the limits the API reports, with a permanent Codex five-hour
//! column so an unreported window is visible as a dash.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::io::IsTerminal;

use jiff::Timestamp;

use crate::model::{Health, Limit, ModelAvailability, Provider, UsageResponse};

/// Width of a usage bar, in cells.
const BAR: usize = 4;

/// The partial fills a bar cell can show, indexed by eighths: nothing, then
/// the left-eighths block glyphs from one eighth to seven. Eight eighths is a
/// full cell, `█`, so it has no entry here.
const EIGHTHS: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];

/// Rendered width of a reset countdown. The longest a limit can be away is a
/// week, so `23h59m` is the widest this gets.
const COUNTDOWN: usize = 6;

/// Rendered width of a populated cell: the bar, a right-aligned percentage, and
/// the countdown to that limit coming back.
const CELL: usize = BAR + 1 + 4 + 1 + COUNTDOWN;

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";

/// What the table needs to know about one account. Deliberately free of stash
/// and API types so the table stays a pure formatter.
#[derive(Clone)]
pub struct Entry {
    pub provider: Provider,
    pub slug: String,
    pub email: String,
    pub plan: String,
    pub active: bool,
    /// The account's limits, or why they could not be read. One field rather
    /// than two, so "has limits" and "failed" cannot both be true at once.
    pub usage: Result<UsageResponse, String>,
}

impl Entry {
    /// The limits that were readable; empty when the probe failed.
    pub fn known(&self) -> &[Limit] {
        self.usage.as_ref().map(|u| u.limits.as_slice()).unwrap_or(&[])
    }

    pub fn models(&self) -> Option<&BTreeMap<String, ModelAvailability>> {
        self.usage.as_ref().ok()?.model_usage.as_ref()
    }

    fn visible_limits(&self) -> impl Iterator<Item = &Limit> {
        self.known()
            .iter()
            .filter(|l| l.model_name().is_none_or(|name| !hidden_model(self.provider, name)))
    }

    fn visible_models(&self) -> impl Iterator<Item = (&str, &ModelAvailability)> {
        self.models()
            .filter(|_| self.provider == Provider::Codex)
            .into_iter()
            .flatten()
            .filter(|(name, _)| !hidden_model(self.provider, name))
            .map(|(name, status)| (name.as_str(), status))
    }

    /// The worst standing across this account's limits, which is what decides
    /// whether it is still worth switching to.
    pub fn health(&self) -> Health {
        if self.visible_models().any(|(_, status)| status.available == Some(false)) {
            return Health::Critical;
        }
        self.visible_limits().map(Limit::health).fold(Health::Ok, |worst, h| match (worst, h) {
            (Health::Critical, _) | (_, Health::Critical) => Health::Critical,
            (Health::Warn, _) | (_, Health::Warn) => Health::Warn,
            _ => Health::Ok,
        })
    }

    /// Reasons to check before selecting this account. Availability is stated
    /// as a gate, never as an invented percentage or a spent quota window.
    pub fn restrictions(&self) -> Vec<String> {
        self.visible_limits()
            .filter(|l| l.exhausted())
            .map(|l| format!("no {} left", self.limit_label(l)))
            .chain(
                self.visible_models()
                    .filter(|(_, status)| status.available == Some(false))
                    .map(|(name, _)| format!("{} unavailable", model_label(name))),
            )
            .collect()
    }

    fn limit_label(&self, limit: &Limit) -> String {
        match self.provider {
            Provider::Claude => limit.column(),
            Provider::Codex => {
                format!("{} {}", limit.model_name().unwrap_or("Shared"), codex_window(limit))
            }
        }
    }
}

#[derive(Clone, Copy)]
pub struct Style {
    color: bool,
}

impl Style {
    /// Colour when stdout is a terminal and the environment has not asked
    /// otherwise.
    pub fn detect() -> Self {
        let suppressed = std::env::var_os("NO_COLOR").is_some();
        Self { color: !suppressed && std::io::stdout().is_terminal() }
    }

    pub fn colored() -> Self {
        Self { color: true }
    }

    fn paint(&self, text: &str, code: &str) -> String {
        if !self.color || code.is_empty() {
            return text.to_string();
        }
        format!("{code}{text}{RESET}")
    }

    pub fn health(&self, text: &str, health: Health) -> String {
        self.paint(
            text,
            match health {
                Health::Ok => "\x1b[32m",
                Health::Warn => "\x1b[33m",
                Health::Critical => "\x1b[31m",
            },
        )
    }

    pub fn dim(&self, text: &str) -> String {
        self.paint(text, DIM)
    }

    pub fn bold(&self, text: &str) -> String {
        self.paint(text, BOLD)
    }

    pub fn accent(&self, text: &str) -> String {
        self.paint(text, "\x1b[36m")
    }

    pub fn selected(&self, text: &str) -> String {
        self.paint(text, "\x1b[1;7m")
    }
}

/// Each provider owns its columns. Codex has multiple quota pools per account,
/// so its pools are rows under one account, with windows across the columns.
struct Section {
    provider: Provider,
    columns: Vec<String>,
    email_width: usize,
    plan_width: usize,
    pool_width: usize,
    models: Vec<String>,
}

pub struct Table {
    sections: Vec<Section>,
    entries: Vec<Entry>,
    style: Style,
}

impl Table {
    pub fn build(mut entries: Vec<Entry>, style: Style) -> Self {
        entries.sort_by_key(|e| e.provider);
        let sections = Provider::ALL
            .into_iter()
            .filter_map(|provider| {
                let accounts: Vec<_> = entries.iter().filter(|e| e.provider == provider).collect();
                if accounts.is_empty() {
                    return None;
                }
                let email_width =
                    accounts.iter().map(|e| e.email.len()).max().unwrap_or(0).max("ACCOUNT".len());
                let plan_width =
                    accounts.iter().map(|e| e.plan.len()).max().unwrap_or(0).max("PLAN".len());
                let pool_width = accounts
                    .iter()
                    .filter(|e| e.provider == Provider::Codex)
                    .flat_map(|e| e.visible_limits())
                    .filter_map(Limit::model_name)
                    .map(str::len)
                    .max()
                    .map(|width| width.max("Shared".len()))
                    .unwrap_or(0);
                let models: BTreeSet<_> = accounts
                    .iter()
                    .flat_map(|e| e.visible_models())
                    .map(|(name, _)| name.to_string())
                    .collect();
                let columns = match provider {
                    Provider::Claude => columns_of(accounts.iter().copied()),
                    Provider::Codex => {
                        let windows =
                            accounts.iter().flat_map(|e| e.visible_limits()).map(codex_window);
                        ordered_columns(std::iter::once("5h".into()).chain(windows))
                    }
                };
                Some(Section {
                    provider,
                    columns,
                    email_width,
                    plan_width,
                    pool_width,
                    models: models.into_iter().collect(),
                })
            })
            .collect();
        Self { sections, entries, style }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Move only the selected provider's active marker.
    pub fn mark_active(&mut self, slug: &str) {
        let provider = self.entries.iter().find(|e| e.slug == slug).map(|e| e.provider);
        for entry in &mut self.entries {
            if provider.is_none_or(|p| entry.provider == p) {
                entry.active = entry.slug == slug;
            }
        }
    }

    /// The same sections in the picker and `ls`. Account indices match stash
    /// resolution; a pool row never acquires a selectable account number.
    pub fn lines(&self, selected: Option<usize>) -> Vec<String> {
        let mut lines = Vec::new();
        for section in &self.sections {
            if !lines.is_empty() {
                lines.push(String::new());
            }
            lines.push(format!("  {}", self.style.bold(section.provider.label())));
            lines.push(format!("  {}", self.header(section)));
            for (index, _) in
                self.entries.iter().enumerate().filter(|(_, e)| e.provider == section.provider)
            {
                for (line, text) in self.row(index).lines().enumerate() {
                    let marker = if line == 0 && selected == Some(index) { "> " } else { "  " };
                    lines.push(format!("{marker}{text}"));
                }
            }
        }
        lines
    }

    fn header(&self, section: &Section) -> String {
        let cells = section
            .columns
            .iter()
            .map(|c| format!("{:<width$}", c.to_uppercase(), width = CELL.max(c.len())))
            .collect::<Vec<_>>()
            .join("  ");
        let pool = match section.pool_width {
            0 => String::new(),
            width => format!("{:<width$}  ", "POOL"),
        };
        let models = section
            .models
            .iter()
            .map(|name| {
                format!(
                    "  {:<width$}",
                    model_label(name).to_uppercase(),
                    width = self.model_width(section, name)
                )
            })
            .collect::<String>();
        self.style.dim(&format!(
            "   {:<ew$}  {:<pw$}  {pool}{cells}{models}",
            "ACCOUNT",
            "PLAN",
            ew = section.email_width,
            pw = section.plan_width
        ))
    }

    /// One account, including its additional Codex pools on continuation rows.
    pub fn row(&self, index: usize) -> String {
        let Some(entry) = self.entries.get(index) else { return String::new() };
        let Some(section) = self.sections.iter().find(|s| s.provider == entry.provider) else {
            return String::new();
        };
        let head = format!(
            "{:>2} {:<ew$}  {:<pw$}",
            index + 1,
            entry.email,
            entry.plan,
            ew = section.email_width,
            pw = section.plan_width
        );
        let head = match entry.health() {
            Health::Critical => self.style.dim(&head),
            _ => head,
        };
        let suffix = if entry.active { self.style.bold("  <- active") } else { String::new() };
        if let Err(error) = &entry.usage {
            return format!("{head}  {}{suffix}", self.style.health(error, Health::Critical));
        }
        let no_readings = section.columns.is_empty() && section.models.is_empty();
        if no_readings {
            return format!("{head}  no usage readings reported{suffix}");
        }
        match entry.provider {
            Provider::Claude => {
                let cells = section
                    .columns
                    .iter()
                    .map(|column| {
                        let limit = entry.known().iter().find(|l| l.column() == *column);
                        self.cell(limit, CELL.max(column.len()))
                    })
                    .collect::<Vec<_>>()
                    .join("  ");
                format!("{head}  {cells}{suffix}")
            }
            Provider::Codex => {
                let named: BTreeSet<_> =
                    entry.visible_limits().filter_map(Limit::model_name).collect();
                let pools = std::iter::once(None).chain(named.into_iter().map(Some));
                pools
                    .enumerate()
                    .map(|(row, pool)| {
                        let cells = section
                            .columns
                            .iter()
                            .map(|column| {
                                let limit = entry
                                    .known()
                                    .iter()
                                    .find(|l| l.model_name() == pool && codex_window(l) == *column);
                                self.cell(limit, CELL.max(column.len()))
                            })
                            .collect::<Vec<_>>()
                            .join("  ");
                        let prefix = match row {
                            0 => head.clone(),
                            _ => " ".repeat(3 + section.email_width + 2 + section.plan_width),
                        };
                        let active = if row == 0 { suffix.as_str() } else { "" };
                        let pool = match section.pool_width {
                            0 => String::new(),
                            width => format!("{:<width$}  ", pool.unwrap_or("Shared")),
                        };
                        let models = match row {
                            0 => section
                                .models
                                .iter()
                                .map(|name| {
                                    let status = entry.models().and_then(|models| models.get(name));
                                    let text = format!(
                                        "  {:<width$}",
                                        availability(status),
                                        width = self.model_width(section, name)
                                    );
                                    match status.and_then(|s| s.available) {
                                        Some(true) => self.style.health(&text, Health::Ok),
                                        Some(false) => self.style.health(&text, Health::Critical),
                                        None => self.style.dim(&text),
                                    }
                                })
                                .collect::<String>(),
                            _ => String::new(),
                        };
                        format!("{prefix}  {pool}{cells}{models}{active}")
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
    }

    fn model_width(&self, section: &Section, name: &str) -> usize {
        self.entries
            .iter()
            .filter(|e| e.provider == section.provider)
            .map(|e| availability(e.models().and_then(|models| models.get(name))).len())
            .max()
            .unwrap_or(0)
            .max(model_label(name).len())
            .max(CELL)
    }

    fn cell(&self, limit: Option<&Limit>, width: usize) -> String {
        let Some(limit) = limit else {
            return self.style.dim(&format!("{:<width$}", "—"));
        };
        let reset = limit.resets_at.as_deref().and_then(until_compact).unwrap_or_default();
        let text = format!(
            "{} {:>3.0}% {reset:<COUNTDOWN$}",
            bar(limit.percent),
            limit.percent.clamp(0.0, 100.0)
        );
        self.style.health(&format!("{text:<width$}"), limit.health())
    }
}

fn hidden_model(provider: Provider, name: &str) -> bool {
    provider == Provider::Codex && name.eq_ignore_ascii_case("gpt-5.3-codex-spark")
}

fn model_label(name: &str) -> &str {
    match name {
        "gpt-6-astra" => "Astra",
        _ => name,
    }
}

pub fn availability(status: Option<&ModelAvailability>) -> String {
    availability_at(status, Timestamp::now())
}

fn availability_at(status: Option<&ModelAvailability>, now: Timestamp) -> String {
    let Some(status) = status else { return "unknown".into() };
    match status.available {
        Some(true) => "available".into(),
        None => "unknown".into(),
        Some(false) => {
            let text = status
                .available_at
                .as_ref()
                .and_then(|at| at.instant())
                .filter(|at| *at > now)
                .map(|at| format!("back {}", compact(at.as_second() - now.as_second())))
                .unwrap_or_else(|| "unavailable".into());
            match status.credits_would_enable {
                Some(true) => format!("{text}; credits unlock"),
                _ => text,
            }
        }
    }
}

fn codex_window(limit: &Limit) -> String {
    match limit.kind.as_str() {
        "session" => "5h".into(),
        // Older caches dropped one of the windows and did not retain its duration.
        "weekly_scoped" => "cached window".into(),
        _ => limit.window(),
    }
}

/// The union of limit columns across every account, ordered so the two limits
/// that always exist lead and per-model weeklies follow by name.
fn columns_of<'a>(entries: impl IntoIterator<Item = &'a Entry>) -> Vec<String> {
    ordered_columns(entries.into_iter().flat_map(|e| e.visible_limits().map(Limit::column)))
}

fn ordered_columns(names: impl Iterator<Item = String>) -> Vec<String> {
    let unique: BTreeSet<String> = names.collect();
    let mut columns: Vec<String> = unique.into_iter().collect();
    columns.sort_by(|a, b| match rank(a).cmp(&rank(b)) {
        Ordering::Equal => a.cmp(b),
        other => other,
    });
    columns
}

fn rank(column: &str) -> u8 {
    match column {
        "session" | "5h" => 0,
        "weekly" => 1,
        _ => 2,
    }
}

/// A usage bar, filled to the eighth of a cell: a cell is full, one of the
/// seven left-eighths glyphs, or track. The fill is rounded down so the bar
/// never claims more than is spent and reads full only at 100%, except that
/// any non-zero usage shows at least one eighth, so a barely-touched limit
/// still reads as touched.
fn bar(percent: f64) -> String {
    let steps = BAR * EIGHTHS.len();
    let exact = (percent.clamp(0.0, 100.0) / 100.0) * steps as f64;
    let touched = exact > 0.0;
    let filled = (exact.floor() as usize).max(usize::from(touched));
    let full = filled / EIGHTHS.len();
    let partial = EIGHTHS[filled % EIGHTHS.len()];
    let track = BAR - full - usize::from(!partial.is_empty());
    format!("{}{partial}{}", "█".repeat(full), "░".repeat(track))
}

/// Time until an RFC 3339 instant, phrased for a glance.
pub fn until(rfc3339: &str) -> Option<String> {
    seconds_until(rfc3339).map(human)
}

/// The same countdown narrowed to fit a table cell.
fn until_compact(rfc3339: &str) -> Option<String> {
    seconds_until(rfc3339).map(compact)
}

fn seconds_until(rfc3339: &str) -> Option<i64> {
    let target: Timestamp = rfc3339.parse().ok()?;
    Some(target.as_second() - Timestamp::now().as_second())
}

/// A duration with the spaces squeezed out, for somewhere a column of them has
/// to line up.
pub fn compact(seconds: i64) -> String {
    phrase(seconds, "")
}

/// What acting on an account is about to do, so the picker names it.
#[derive(Clone, Copy)]
pub enum Verb {
    /// Replace the live credentials, which every session follows.
    Switch,
    /// Start one session on the account, leaving every other where it is.
    Launch,
}

impl Verb {
    /// The verb alone, as the footer's list of keys says it.
    pub fn word(self) -> &'static str {
        match self {
            Self::Switch => "switch",
            Self::Launch => "launch",
        }
    }
}

/// The question to put before acting on an account, naming what is spent when
/// something is. Shared so the picker and the command line ask it the same way.
pub fn question(entry: &Entry, verb: Verb) -> String {
    let account = format!("{} on {}", entry.email, entry.provider.label());
    let (asked, anyway) = match verb {
        Verb::Switch => (format!("Switch to {account}?"), "Switch anyway?"),
        Verb::Launch => (
            format!("Launch a {} session on {}?", entry.provider.label(), entry.email),
            "Launch anyway?",
        ),
    };
    let restrictions = entry.restrictions();
    match restrictions.is_empty() {
        true => asked,
        false => format!("{account}: {}. {anyway}", restrictions.join("; ")),
    }
}

fn human(seconds: i64) -> String {
    phrase(seconds, " ")
}

/// A duration in the largest two units that say anything, `gap` between them.
/// Prose has room for the space; a column of these has to line up without it.
fn phrase(seconds: i64, gap: &str) -> String {
    if seconds <= 0 {
        return "now".to_string();
    }
    let (hours, minutes) = (seconds / 3600, (seconds % 3600) / 60);
    match (hours, minutes) {
        (0, 0) => format!("{seconds}s"),
        (0, m) => format!("{m}m"),
        (h, m) if h < 24 => format!("{h}h{gap}{m:02}m"),
        (h, _) => format!("{}d{gap}{}h", h / 24, h % 24),
    }
}

/// Per-limit detail for one account: where each limit stands and when it comes
/// back, for `ccs status`.
pub fn detail(entry: &Entry, style: Style) -> Vec<String> {
    if let Err(error) = &entry.usage {
        return vec![style.health(&format!("  {error}"), Health::Critical)];
    }
    let width = entry.visible_limits().map(|l| entry.limit_label(l).len()).max().unwrap_or(0);
    let mut lines: Vec<_> = entry
        .visible_limits()
        .map(|limit| {
            let resets = limit
                .resets_at
                .as_deref()
                .and_then(until)
                .map(|d| format!("resets in {d}"))
                .unwrap_or_default();
            let note = if limit.exhausted() { "   spent" } else { "" };
            let body = format!(
                "  {:<width$} {} {:>3.0}%   {resets}{note}",
                entry.limit_label(limit),
                bar(limit.percent),
                limit.percent.clamp(0.0, 100.0),
            );
            style.health(&body, limit.health())
        })
        .collect();
    lines.extend(entry.visible_models().map(|(name, status)| {
        let text = format!("  {}: {}", model_label(name), availability(Some(status)));
        match status.available {
            Some(true) => style.health(&text, Health::Ok),
            Some(false) => style.health(&text, Health::Critical),
            None => style.dim(&text),
        }
    }));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limit;

    fn plain() -> Style {
        Style { color: false }
    }

    fn entry(email: &str, limits: Vec<Limit>) -> Entry {
        Entry {
            provider: Provider::Claude,
            slug: email.replace('@', "_at_"),
            email: email.into(),
            plan: "max20x".into(),
            active: false,
            usage: Ok(limits.into()),
        }
    }

    #[test]
    fn a_bar_is_empty_only_at_genuine_zero() {
        assert_eq!(bar(0.0), "░░░░");
        assert_eq!(bar(0.4), "▏░░░");
    }

    #[test]
    fn a_bar_rounds_down_to_the_eighth() {
        assert_eq!(bar(6.0), "▏░░░");
        assert_eq!(bar(43.0), "█▋░░");
        assert_eq!(bar(50.0), "██░░");
        assert_eq!(bar(51.0), "██░░");
        assert_eq!(bar(89.0), "███▌");
    }

    #[test]
    fn a_bar_is_full_only_at_full_and_clamps_past_it() {
        assert_eq!(bar(97.0), "███▉");
        assert_eq!(bar(100.0), "████");
        assert_eq!(bar(220.0), "████");
    }

    /// Which models carry a scoped limit is the endpoint's to decide and no
    /// part of this is fixed to a roster, so the names here are arbitrary: what
    /// is under test is where a scoped column lands, not what it is called.
    #[test]
    fn columns_lead_with_the_two_families_that_always_exist() {
        let entries = vec![entry(
            "a@x.com",
            vec![
                limit!("weekly_scoped", 10.0, model = "Zephyr"),
                limit!("weekly_all", 53.0),
                limit!("session", 3.0),
                limit!("weekly_scoped", 100.0, model = "Fable"),
            ],
        )];
        assert_eq!(columns_of(&entries), ["session", "weekly", "Fable", "Zephyr"]);
    }

    #[test]
    fn columns_are_the_union_across_accounts() {
        let entries = vec![
            entry("a@x.com", vec![limit!("weekly_scoped", 1.0, model = "Fable")]),
            entry("b@x.com", vec![limit!("weekly_scoped", 2.0, model = "Zephyr")]),
        ];
        assert_eq!(columns_of(&entries), ["Fable", "Zephyr"]);
    }

    #[test]
    fn providers_have_separate_columns_and_codex_pools_keep_both_windows() {
        let claude = entry("z@claude.test", vec![limit!("weekly_scoped", 12.0, model = "Fable")]);
        let mut codex = entry(
            "a@codex.test",
            vec![
                limit!("session", 10.0),
                limit!("weekly_all", 20.0),
                limit!("session", 30.0, model = "Another pool"),
                limit!("weekly_all", 50.0, model = "Another pool"),
            ],
        );
        codex.provider = Provider::Codex;
        let table = Table::build(vec![codex, claude], plain());
        let screen = table.lines(Some(1)).join("\n");
        let (claude, codex) = screen.split_once("  Codex\n").expect("Codex section");
        assert!(claude.contains("Claude Code"));
        assert!(claude.contains("FABLE"));
        assert!(!claude.contains("POOL"));
        assert!(!claude.contains("Spark"));
        assert!(codex.contains("POOL"));
        assert!(codex.contains("5H"));
        assert!(codex.contains("WEEKLY"));
        assert!(!codex.contains("FABLE"));
        assert!(codex.contains(">  2 a@codex.test"));
        let shared = codex.lines().find(|l| l.contains("Shared")).unwrap();
        assert!(shared.contains("10%") && shared.contains("20%"));
        let other = codex.lines().find(|l| l.contains("Another pool")).unwrap();
        assert!(other.contains("30%") && other.contains("50%"));
        assert_eq!(table.len(), 2, "pool rows do not become selectable accounts");
        assert!(question(&table.entries()[1], Verb::Switch).contains("on Codex"));
    }

    fn with_models(mut entry: Entry, raw: serde_json::Value) -> Entry {
        entry.provider = Provider::Codex;
        entry.usage.as_mut().unwrap().model_usage = Some(serde_json::from_value(raw).unwrap());
        entry
    }

    #[test]
    fn model_gates_get_columns_without_inventing_percentages_or_availability() {
        let available = with_models(
            entry("a@x", vec![]),
            serde_json::json!({
                "gpt-6-astra": {"available": true}, "future-model": {"available": false}
            }),
        );
        let blocked = with_models(
            entry("b@x", vec![]),
            serde_json::json!({
                "gpt-6-astra": {"available": false, "credits_would_enable": true}
            }),
        );
        assert_eq!(blocked.health(), Health::Critical);
        assert_eq!(blocked.restrictions(), ["Astra unavailable"]);
        assert!(question(&blocked, Verb::Switch).contains("Astra unavailable. Switch anyway?"));
        let mut missing = entry("c@x", vec![]);
        missing.provider = Provider::Codex;
        let table = Table::build(vec![available, blocked, missing], plain());
        let screen = table.lines(None).join("\n");
        assert!(screen.contains("ASTRA") && screen.contains("FUTURE-MODEL"));
        assert!(!screen.contains('%') && !screen.contains("POOL"));
        assert!(table.row(0).contains("available"));
        assert!(table.row(1).contains("unavailable; credits unlock"));
        assert!(table.row(2).contains("unknown"));
        assert!(table.entries()[2].restrictions().is_empty());
        assert_eq!(detail(&table.entries()[1], plain()), ["  Astra: unavailable; credits unlock"]);
    }

    #[test]
    fn a_model_countdown_never_overrides_the_reported_availability() {
        let now: Timestamp = "2026-09-07T00:00:00Z".parse().unwrap();
        for at in [serde_json::json!("2026-09-07T01:30:00Z"), serde_json::json!(1788744600_i64)] {
            let status: ModelAvailability = serde_json::from_value(serde_json::json!({
                "available": false, "available_at": at, "credits_would_enable": true
            }))
            .unwrap();
            assert_eq!(availability_at(Some(&status), now), "back 1h30m; credits unlock");
        }
        for at in [serde_json::json!("2026-09-06T23:00:00Z"), serde_json::json!("unreadable")] {
            let mut status: ModelAvailability = serde_json::from_value(serde_json::json!({
                "available": false, "available_at": at
            }))
            .unwrap();
            assert_eq!(availability_at(Some(&status), now), "unavailable");
            status.available = Some(true);
            assert_eq!(availability_at(Some(&status), now), "available");
            status.available = None;
            assert_eq!(availability_at(Some(&status), now), "unknown");
        }
    }

    #[test]
    fn spark_is_preserved_as_data_but_does_not_warn_or_clutter_the_display() {
        let codex = with_models(
            entry(
                "a@x",
                vec![
                    limit!("weekly_all", 10.0),
                    limit!("session", 100.0, model = "GPT-5.3-Codex-Spark"),
                ],
            ),
            serde_json::json!({"gpt-5.3-codex-spark": {"available": false}}),
        );
        assert_eq!(codex.known().len(), 2);
        assert_eq!(codex.models().unwrap().len(), 1);
        assert_eq!(codex.health(), Health::Ok);
        assert!(codex.restrictions().is_empty());
        assert!(!question(&codex, Verb::Switch).contains("anyway"));
        assert!(!detail(&codex, plain()).join("\n").contains("Spark"));
        let screen = Table::build(vec![codex], plain()).lines(None).join("\n");
        assert!(screen.contains("10%"));
        assert!(!screen.to_lowercase().contains("spark"));
        assert!(!screen.contains("POOL") && screen.contains("5H"));
        assert!(screen.contains('—'), "the shared five-hour reading is unreported");
        assert!(!screen.contains("100%"), "Spark cannot fill the shared five-hour cell");
    }

    #[test]
    fn switching_in_a_provider_section_preserves_the_other_active_account() {
        let mut claude = entry("claude@x", vec![]);
        claude.active = true;
        let mut codex = entry("codex@x", vec![]);
        codex.provider = Provider::Codex;
        let mut table = Table::build(vec![claude, codex], plain());
        table.mark_active("codex_at_x");
        assert!(table.entries().iter().all(|e| e.active));
    }

    #[test]
    fn old_codex_caches_do_not_guess_the_duration_of_the_window_they_kept() {
        let mut codex = entry("a@x", vec![limit!("weekly_scoped", 42.0, model = "Old pool")]);
        codex.provider = Provider::Codex;
        let screen = Table::build(vec![codex], plain()).lines(None).join("\n");
        assert!(screen.contains("CACHED WINDOW"));
        assert!(screen.contains("42%"));
        assert!(!screen.contains("WEEKLY"));
    }

    #[test]
    fn an_account_is_as_healthy_as_its_worst_limit() {
        let spent = entry(
            "a@x.com",
            vec![limit!("session", 3.0), limit!("weekly_scoped", 100.0, model = "Fable")],
        );
        assert_eq!(spent.health(), Health::Critical);
        assert_eq!(spent.restrictions(), ["no Fable left"]);
    }

    #[test]
    fn an_unreadable_account_reports_no_limits_rather_than_pretending() {
        let broken = Entry {
            provider: Provider::Claude,
            slug: "a".into(),
            email: "a@x.com".into(),
            plan: "?".into(),
            active: false,
            usage: Err("token rejected".into()),
        };
        assert!(broken.known().is_empty());
        assert!(broken.restrictions().is_empty());
    }

    #[test]
    fn a_row_shows_the_failure_instead_of_empty_bars() {
        let broken = Entry {
            provider: Provider::Claude,
            slug: "a".into(),
            email: "a@x.com".into(),
            plan: "?".into(),
            active: false,
            usage: Err("token rejected".into()),
        };
        let table = Table::build(vec![broken], plain());
        assert!(table.row(0).contains("token rejected"));
    }

    #[test]
    fn a_row_that_could_not_be_read_still_says_it_is_the_account_in_use() {
        let broken = Entry {
            provider: Provider::Claude,
            slug: "a".into(),
            email: "a@x.com".into(),
            plan: "?".into(),
            active: true,
            usage: Err("token rejected".into()),
        };
        let row = Table::build(vec![broken], plain()).row(0);
        assert!(row.contains("token rejected"), "{row}");
        assert!(row.contains("<- active"), "{row}");
    }

    #[test]
    fn a_row_marks_the_active_account_and_dashes_limits_it_lacks() {
        let mut active = entry("a@x.com", vec![limit!("session", 3.0)]);
        active.active = true;
        let other = entry("b@x.com", vec![limit!("weekly_scoped", 4.0, model = "Fable")]);
        let table = Table::build(vec![active, other], plain());

        let first = table.row(0);
        assert!(first.contains("<- active"), "{first}");
        assert!(
            first.contains("—"),
            "row should dash the Fable column it has no limit for: {first}"
        );
        assert!(!table.row(1).contains("<- active"));
    }

    #[test]
    fn durations_read_at_a_glance() {
        assert_eq!(human(-5), "now");
        assert_eq!(human(45), "45s");
        assert_eq!(human(9 * 60), "9m");
        assert_eq!(human(4 * 3600 + 12 * 60), "4h 12m");
        assert_eq!(human(50 * 3600), "2d 2h");
    }

    #[test]
    fn a_reset_already_past_reads_as_now() {
        assert_eq!(until("2020-01-01T00:00:00+00:00").as_deref(), Some("now"));
    }

    #[test]
    fn compact_durations_squeeze_out_the_spaces_to_fit_a_column() {
        assert_eq!(compact(-5), "now");
        assert_eq!(compact(45), "45s");
        assert_eq!(compact(9 * 60), "9m");
        assert_eq!(compact(4 * 3600 + 12 * 60), "4h12m");
        assert_eq!(compact(50 * 3600), "2d2h");
    }

    #[test]
    fn a_compact_duration_never_outgrows_its_column() {
        let widest = [0, 59, 60, 3599, 3600, 23 * 3600 + 59 * 60, 6 * 86400 + 23 * 3600];
        for seconds in widest {
            let rendered = compact(seconds);
            assert!(rendered.chars().count() <= COUNTDOWN, "{seconds}s renders as {rendered}");
        }
    }

    #[test]
    fn a_cell_carries_the_countdown_beside_the_percentage() {
        let past = "2020-01-01T00:00:00+00:00";
        let entries = vec![entry("a@x.com", vec![limit!("session", 50.0, resets = past)])];
        let row = Table::build(entries, plain()).row(0);
        assert!(row.contains("50%"), "{row}");
        assert!(row.contains("now"), "{row}");
    }

    #[test]
    fn the_question_names_what_is_spent() {
        let fine = entry("a@x.com", vec![limit!("session", 3.0)]);
        assert_eq!(question(&fine, Verb::Switch), "Switch to a@x.com on Claude Code?");

        let spent = entry("a@x.com", vec![limit!("weekly_scoped", 100.0, model = "Fable")]);
        let asked = question(&spent, Verb::Switch);
        assert!(asked.contains("no Fable left"), "{asked}");
        assert!(asked.contains("Switch anyway?"), "{asked}");
    }

    #[test]
    fn a_launch_is_never_described_as_a_switch() {
        let fine = entry("a@x.com", vec![limit!("session", 3.0)]);
        assert_eq!(question(&fine, Verb::Launch), "Launch a Claude Code session on a@x.com?");

        let spent = entry("a@x.com", vec![limit!("weekly_scoped", 100.0, model = "Fable")]);
        let asked = question(&spent, Verb::Launch);
        assert!(asked.contains("no Fable left"), "{asked}");
        assert!(asked.contains("Launch anyway?"), "{asked}");
        assert!(!asked.contains("Switch"), "a launch moves no other session: {asked}");
    }

    #[test]
    fn an_unparseable_reset_is_dropped_rather_than_guessed() {
        assert_eq!(until("not a timestamp"), None);
    }

    #[test]
    fn the_active_marker_follows_the_account_that_was_installed() {
        let mut table = Table::build(
            vec![entry("a@x.com", vec![limit!("session", 3.0)]), entry("b@x.com", vec![])],
            plain(),
        );
        table.mark_active("b_at_x.com");

        assert!(!table.row(0).contains("<- active"), "{}", table.row(0));
        assert!(table.row(1).contains("<- active"), "{}", table.row(1));
    }

    #[test]
    fn marking_one_account_active_takes_the_marker_off_every_other() {
        let mut first = entry("a@x.com", vec![limit!("session", 3.0)]);
        first.active = true;
        let mut table = Table::build(vec![first, entry("b@x.com", vec![])], plain());
        table.mark_active("b_at_x.com");

        assert!(!table.row(0).contains("<- active"), "{}", table.row(0));
        assert!(table.row(1).contains("<- active"), "{}", table.row(1));
    }

    #[test]
    fn marking_an_account_the_table_does_not_hold_leaves_it_unmarked() {
        let mut active = entry("a@x.com", vec![limit!("session", 3.0)]);
        active.active = true;
        let mut table = Table::build(vec![active], plain());
        table.mark_active("nobody");

        assert!(!table.row(0).contains("<- active"), "{}", table.row(0));
    }
}
