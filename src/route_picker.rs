//! Terminal model routing: provider-native catalogs and ordered account choices.

use anyhow::{Context, Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal;

use crate::cmd::{self, Ctx};
use crate::model::{Health, Provider};
use crate::picker::{self, Screen};
use crate::render::Style;
use crate::routing::{Routing, Rule};

pub fn run(ctx: &Ctx, provider: Option<Provider>) -> Result<()> {
    let screen = Screen::enter()?;
    let result = edit(ctx, provider)?;
    drop(screen);
    if let Some(message) = result {
        println!("{}", Style::detect().health(&message, Health::Ok));
    }
    Ok(())
}

/// The account picker already owns the terminal; do not enter a nested screen.
pub(crate) fn edit(ctx: &Ctx, provider: Option<Provider>) -> Result<Option<String>> {
    let provider = match provider {
        Some(provider) => provider,
        None => {
            let choices = Provider::ALL.map(|p| (p.label(), p));
            let Some(provider) =
                picker::select_in_screen("Model route — choose provider", &choices)?
            else {
                return Ok(None);
            };
            provider
        }
    };
    let accounts: Vec<_> = ctx
        .stash
        .list()?
        .into_iter()
        .filter(|a| a.account.provider == provider)
        .map(|a| (a.slug, a.account.email))
        .collect();
    if accounts.is_empty() {
        bail!("no {provider} accounts; run ccs add --{provider}");
    }
    let mut menu = Models::default();
    menu.reload(ctx, provider)?;
    loop {
        let Some(model) = choose_model(&mut menu, ctx, provider)? else {
            return Ok(None);
        };
        let routes = Routing::read(ctx.stash.root())?;
        let selected = routes
            .rules
            .iter()
            .find(|r| r.provider == provider && r.model == model)
            .map(|r| r.accounts.clone())
            .unwrap_or_default();
        let mut choices = AccountChoices { rows: accounts.clone(), selected, at: 0 };
        // Keep missing accounts visible so a broken existing route can be repaired.
        for slug in &choices.selected {
            if !choices.rows.iter().any(|(s, _)| s == slug) {
                choices.rows.push((slug.clone(), format!("{slug} (missing — deselect)")));
            }
        }
        let mut note = String::new();
        loop {
            let rows: Vec<_> = choices
                .rows
                .iter()
                .map(|(slug, email)| {
                    let order = choices.selected.iter().position(|s| s == slug);
                    match order {
                        Some(0) => Row(format!("[1] {email} ({slug}) — primary"), Tone::Primary),
                        Some(n) => {
                            Row(format!("[{}] {email} ({slug}) — fallback", n + 1), Tone::Fallback)
                        }
                        None => Row(format!("[ ] {email} ({slug})"), Tone::Normal),
                    }
                })
                .collect();
            draw(
                &format!("Model route — {provider}"),
                &model,
                &rows,
                choices.at,
                &note,
                [
                    "up/down select   space toggle   left/right change priority",
                    "enter save   esc back   ctrl-c cancel",
                ],
            )?;
            let Some(key) = key()? else {
                continue;
            };
            if interrupted(key) {
                return Ok(None);
            }
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => break,
                KeyCode::Char(' ') => choices.toggle(),
                KeyCode::Left => choices.reorder(false),
                KeyCode::Right => choices.reorder(true),
                KeyCode::Enter => {
                    let rule =
                        Rule { provider, model: model.clone(), accounts: choices.selected.clone() };
                    match cmd::save_route(ctx, rule) {
                        Ok(()) => {
                            return Ok(Some(format!(
                                "Saved {provider} {model}: {}",
                                choices.selected.join(" → ")
                            )));
                        }
                        Err(error) => note = format!("Could not save: {error}"),
                    }
                }
                _ => navigate(key.code, &mut choices.at, choices.rows.len()),
            }
        }
    }
}

#[derive(Default)]
struct Models {
    ids: Vec<String>,
    saved: Vec<String>,
    filter: String,
    at: usize,
    note: String,
}

impl Models {
    fn filtered(&self) -> Vec<String> {
        let mut ids = self.ids.clone();
        for id in &self.saved {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
        let filter = self.filter.to_lowercase();
        ids.retain(|id| id.to_lowercase().contains(&filter));
        ids
    }

    fn replace(&mut self, result: Result<Vec<String>>) {
        let selected = self.filtered().get(self.at).cloned();
        match result {
            Ok(ids) => {
                self.ids = ids;
                self.note.clear();
            }
            Err(error) => {
                self.note = format!("Model lookup failed: {error}; r retries, i enters an ID")
            }
        }
        let filtered = self.filtered();
        self.at = selected
            .and_then(|id| filtered.iter().position(|m| *m == id))
            .unwrap_or_else(|| self.at.min(filtered.len().saturating_sub(1)));
    }

    fn reload(&mut self, ctx: &Ctx, provider: Provider) -> Result<()> {
        draw(&format!("Model route — {provider}"), "Loading models…", &[], 0, "", ["", ""])?;
        self.saved = Routing::read(ctx.stash.root())?
            .rules
            .into_iter()
            .filter(|r| r.provider == provider)
            .map(|r| r.model)
            .collect();
        self.replace(cmd::model_ids(ctx, provider));
        Ok(())
    }
}

fn choose_model(menu: &mut Models, ctx: &Ctx, provider: Provider) -> Result<Option<String>> {
    loop {
        let filtered = menu.filtered();
        let rows: Vec<_> = filtered
            .iter()
            .map(|id| {
                if menu.saved.contains(id) {
                    Row(format!("{id}  [saved]"), Tone::Saved)
                } else {
                    Row(id.clone(), Tone::Normal)
                }
            })
            .collect();
        let subtitle = if menu.filter.is_empty() {
            "Select a model; existing routes are marked [saved]".into()
        } else {
            format!("Filter: {}", menu.filter)
        };
        let note = if rows.is_empty() && menu.note.is_empty() {
            "No matching models. Clear the filter or enter an ID."
        } else {
            &menu.note
        };
        draw(
            &format!("Model route — {provider}"),
            &subtitle,
            &rows,
            menu.at,
            note,
            [
                "up/down select   enter choose   / filter   i enter model ID",
                "r reload models   esc/q back",
            ],
        )?;
        let Some(key) = key()? else {
            continue;
        };
        if interrupted(key) {
            return Ok(None);
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
            KeyCode::Char('r') => menu.reload(ctx, provider)?,
            KeyCode::Char('/') => {
                if let Some(filter) = input("Filter models", &menu.filter)? {
                    menu.filter = filter;
                    menu.at = 0;
                }
            }
            KeyCode::Char('i') => {
                if let Some(model) = input("Exact model ID or prefix ending in *", "")?
                    && !model.trim().is_empty()
                {
                    return Ok(Some(model.trim().into()));
                }
            }
            KeyCode::Enter => {
                if let Some(id) = filtered.get(menu.at) {
                    return Ok(Some(id.clone()));
                }
            }
            _ => navigate(key.code, &mut menu.at, filtered.len()),
        }
    }
}

struct AccountChoices {
    rows: Vec<(String, String)>,
    selected: Vec<String>,
    at: usize,
}

impl AccountChoices {
    fn toggle(&mut self) {
        let Some((slug, _)) = self.rows.get(self.at) else {
            return;
        };
        if let Some(index) = self.selected.iter().position(|s| s == slug) {
            self.selected.remove(index);
        } else {
            self.selected.push(slug.clone());
        }
    }

    fn reorder(&mut self, later: bool) {
        let Some((slug, _)) = self.rows.get(self.at) else {
            return;
        };
        let Some(index) = self.selected.iter().position(|s| s == slug) else {
            return;
        };
        let next =
            if later { (index + 1).min(self.selected.len() - 1) } else { index.saturating_sub(1) };
        self.selected.swap(index, next);
    }
}

fn input(title: &str, initial: &str) -> Result<Option<String>> {
    let mut text = initial.to_string();
    loop {
        draw(
            title,
            &format!("> {text}▏"),
            &[],
            0,
            "",
            ["enter accept   esc cancel   ctrl-u clear", ""],
        )?;
        let Some(key) = key()? else {
            continue;
        };
        if interrupted(key) || key.code == KeyCode::Esc {
            return Ok(None);
        }
        match key.code {
            KeyCode::Enter => return Ok(Some(text)),
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => text.clear(),
            KeyCode::Char(c)
                if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                text.push(c)
            }
            _ => {}
        }
    }
}

fn key() -> Result<Option<KeyEvent>> {
    match event::read().context("reading route key")? {
        Event::Key(key) if key.kind == KeyEventKind::Press => Ok(Some(key)),
        _ => Ok(None),
    }
}

fn interrupted(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn navigate(key: KeyCode, at: &mut usize, len: usize) {
    let last = len.saturating_sub(1);
    match key {
        KeyCode::Up | KeyCode::Char('k') => *at = at.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => *at = (*at + 1).min(last),
        KeyCode::Home => *at = 0,
        KeyCode::End => *at = last,
        _ => {}
    }
}

enum Tone {
    Normal,
    Primary,
    Fallback,
    Saved,
}

struct Row(String, Tone);

fn draw(
    title: &str,
    subtitle: &str,
    rows: &[Row],
    at: usize,
    note: &str,
    hints: [&str; 2],
) -> Result<()> {
    picker::paint(&frame(
        title,
        subtitle,
        rows,
        at,
        note,
        hints,
        terminal::size().unwrap_or((80, 24)),
    ))
}

fn frame(
    title: &str,
    subtitle: &str,
    rows: &[Row],
    at: usize,
    note: &str,
    hints: [&str; 2],
    size: (u16, u16),
) -> String {
    let style = Style::detect();
    // Clip and sanitize data before adding our own terminal styling.
    let clip = |text: &str| {
        text.chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .take(usize::from(size.0.saturating_sub(1)))
            .collect::<String>()
    };
    let count = usize::from(size.1.saturating_sub(9)).max(1);
    let start = at.saturating_sub(count - 1);
    let end = (start + count).min(rows.len());
    let mut lines = vec![
        style.bold(&clip(&format!("  {title}"))),
        style.accent(&clip(&format!("  {subtitle}"))),
        String::new(),
    ];
    for (i, row) in rows.iter().enumerate().take(end).skip(start) {
        let text = clip(&format!("  {} {}", if i == at { ">" } else { " " }, row.0));
        let text = match row.1 {
            Tone::Normal => text,
            Tone::Primary => style.health(&text, Health::Ok),
            Tone::Fallback => style.health(&text, Health::Warn),
            Tone::Saved => style.accent(&text),
        };
        lines.push(if i == at { style.selected(&text) } else { text });
    }
    lines.push(style.dim(&clip(&format!(
        "  {} / {}",
        if rows.is_empty() { 0 } else { at + 1 },
        rows.len()
    ))));
    lines.push(String::new());
    lines.push(style.health(&clip(&format!("  {note}")), Health::Warn));
    lines.extend(hints.map(|hint| style.dim(&clip(&format!("  {hint}")))));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_refresh_keeps_selection_saved_ids_and_last_success_on_failure() {
        let mut menu = Models {
            ids: vec!["new-v1".into(), "new-v2".into()],
            saved: vec!["legacy-*".into()],
            at: 1,
            ..Default::default()
        };
        menu.replace(Ok(vec!["new-v2".into(), "future-v3".into()]));
        assert_eq!(menu.at, 0);
        assert_eq!(menu.filtered(), ["new-v2", "future-v3", "legacy-*"]);
        menu.replace(Err(anyhow::anyhow!("offline")));
        assert_eq!(menu.filtered(), ["new-v2", "future-v3", "legacy-*"]);
        assert!(menu.note.contains("offline"));
        menu.filter = "FUTURE".into();
        assert_eq!(menu.filtered(), ["future-v3"]);
    }

    #[test]
    fn account_order_follows_selection_and_can_change_without_switching() {
        let mut choices = AccountChoices {
            rows: vec![("a".into(), "a@x".into()), ("b".into(), "b@x".into())],
            selected: vec![],
            at: 1,
        };
        choices.toggle();
        choices.at = 0;
        choices.toggle();
        assert_eq!(choices.selected, ["b", "a"]);
        choices.reorder(false);
        assert_eq!(choices.selected, ["a", "b"]);
        choices.toggle();
        assert_eq!(choices.selected, ["b"]);
    }

    #[test]
    fn long_catalog_scrolls_and_cannot_inject_terminal_control_sequences() {
        let rows: Vec<_> =
            (0..100).map(|i| Row(format!("model-{i}\x1b[2J"), Tone::Normal)).collect();
        let frame =
            frame("Models", "Claude", &rows, 99, "", ["enter choose", "esc cancel"], (40, 16));
        assert!(frame.contains("> model-99"));
        assert!(!frame.contains("\x1b"));
        assert!(frame.lines().count() < 16);
        assert!(frame.lines().all(|line| line.chars().count() < 40));
    }
}
