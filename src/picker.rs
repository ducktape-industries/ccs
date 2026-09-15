//! Interactive account picker.
//!
//! The picker owns the screen for its whole lifetime: both re-polling and
//! acting on an account happen through an injected collaborator rather than by
//! exiting and being re-entered, so the list stays up through either. A switch
//! is something done *to* the list — the marker moves and the same table is
//! still there, free to be used again.

use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{self, ClearType};
use crossterm::{cursor, execute, queue};

use crate::model::{Health, Provider};
use crate::render::{self, Style, Table, Verb};

/// How the picker was closed.
pub enum Outcome {
    /// Left for the caller to act on the account with this slug, which is what
    /// an act the picker cannot survive comes back as. Carrying the slug rather
    /// than a row index keeps the answer meaningful after a refresh has rebuilt
    /// the table underneath it.
    Chose(String),
    Quit,
}

/// The accounts on screen, and what the picker may do with one.
///
/// One collaborator rather than a pair of closures because both halves reach
/// the same stash, and only one thing may borrow it at a time.
pub trait Accounts {
    /// Where every account stands.
    fn poll(&mut self) -> Result<Table>;

    /// Act on the account named by `slug`, whatever acting means to the caller.
    fn act(&mut self, slug: &str) -> Result<Act>;

    fn edit_route(&mut self, _provider: Option<Provider>) -> Result<Option<String>> {
        Ok(None)
    }
}

/// What acting on an account leaves the picker doing.
pub enum Act {
    /// Done, and the picker stays up with this to say for it. Nothing but the
    /// active marker moves: installing an account spends nobody's limits, so
    /// the readings on screen are as true afterwards as they were before.
    Installed(String),
    /// The picker is over: the process itself is being handed to the account,
    /// so there is nothing to come back to.
    Handed,
}

/// What the footer is saying, and whether keys mean what they usually mean.
enum Mode {
    Browsing,
    /// Waiting on a yes or no for the account in this row.
    Confirming(usize),
    /// A transient line: work in progress, or a refresh that did not land.
    /// Cosmetic only — every key still does its usual job.
    Note(String),
}

/// Restores the terminal however the picker exits, panic included.
pub(crate) struct Screen;

impl Screen {
    pub(crate) fn enter() -> Result<Self> {
        terminal::enable_raw_mode().context("entering raw mode")?;
        let screen = Self;
        execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide)
            .context("entering alternate screen")?;
        Ok(screen)
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), cursor::Show, terminal::LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

pub fn interactive() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

pub fn provider(action: &str) -> Result<Option<Provider>> {
    select(action, &Provider::ALL.map(|p| (p.label(), p)))
}

/// A short choice before a command starts. Cancellation leaves the command
/// unstarted; the screen is restored before a login child takes the terminal.
pub fn select<T: Copy>(title: &str, choices: &[(&str, T)]) -> Result<Option<T>> {
    if choices.is_empty() {
        return Ok(None);
    }
    let _screen = Screen::enter()?;
    select_in_screen(title, choices)
}

pub(crate) fn select_in_screen<T: Copy>(title: &str, choices: &[(&str, T)]) -> Result<Option<T>> {
    if choices.is_empty() {
        return Ok(None);
    }
    let style = Style::colored();
    let mut at = 0;
    loop {
        let mut lines = vec![format!("  {}", style.bold(title)), String::new()];
        for (index, (label, _)) in choices.iter().enumerate() {
            let marker = if index == at { ">" } else { " " };
            lines.push(format!("  {marker} {label}"));
        }
        lines.push(String::new());
        lines.push(style.dim("  up/down select   enter choose   esc/q cancel"));
        paint(&lines.join("\n"))?;
        let Event::Key(key) = event::read().context("reading provider choice")? else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match decide(key, Some(at), choices.len()) {
            Step::Move(next) => at = next,
            Step::Confirm(index) => return Ok(Some(choices[index].1)),
            Step::Unselect | Step::Quit => return Ok(None),
            Step::Refresh | Step::Routes | Step::Ignore => {}
        }
    }
}

/// How often the list re-polls on its own.
///
/// Each poll costs one request per stashed account, against an endpoint that
/// rate-limits — so the interval is generous, and the countdowns carry the
/// time in between. They are computed from the reset instants at every
/// repaint rather than at every poll, so they keep running down whether or
/// not anything has been fetched; only the percentages wait on a reading.
const AUTO_REFRESH: Duration = Duration::from_secs(600);

/// How long after a keypress an unattended poll holds off. Polling blocks for
/// as long as the network takes, so it waits for a lull rather than freezing
/// the list under someone mid-navigation.
const IDLE_GRACE: Duration = Duration::from_secs(2);

/// How long to wait on a key before looking at the clock again.
const TICK: Duration = Duration::from_millis(200);

/// Show the accounts and let them be acted on. Usage is polled through
/// `accounts`, with the screen already up — the first load, at a long interval
/// after that, and any time `r` is pressed — so the list is never taken away to
/// fetch.
///
/// `verb` is what choosing will do, which the confirmation and the key list
/// both have to say plainly: a switch moves every session, a launch moves none.
pub fn run(accounts: &mut dyn Accounts, verb: Verb) -> Result<Outcome> {
    let _screen = Screen::enter()?;
    let style = Style::colored();

    note(style, "polling accounts…")?;
    let mut table = accounts.poll()?;
    if table.entries().is_empty() {
        return Ok(Outcome::Quit);
    }

    let mut at = Some(0);
    let mut mode = Mode::Browsing;
    let mut polled = Instant::now();
    let mut attempted = Instant::now();
    let mut last_key = Instant::now();
    let mut painted = String::new();

    loop {
        // Repaint only on a real change, so the ticking age in the footer costs
        // one frame a second and nothing else costs any.
        let current = frame(&table, at, &mode, style, polled, verb);
        if current != painted {
            paint(&current)?;
            painted = current;
        }

        let mut poll_now = false;
        if event::poll(TICK).context("waiting for a key")? {
            let Event::Key(key) = event::read().context("reading key")? else { continue };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            last_key = Instant::now();

            // A confirmation is modal: nothing else is listening until it is answered.
            if let Mode::Confirming(target) = mode {
                match answer(key) {
                    Answer::Yes => {
                        let Some(entry) = table.entries().get(target) else {
                            return Ok(Outcome::Quit);
                        };
                        let slug = entry.slug.clone();
                        mode = match accounts.act(&slug) {
                            Ok(Act::Handed) => return Ok(Outcome::Chose(slug)),
                            Ok(Act::Installed(said)) => {
                                table.mark_active(&slug);
                                Mode::Note(said)
                            }
                            // The list is still true and still usable, so a
                            // failure is said and left there rather than thrown
                            // out through a screen about to be torn down.
                            Err(e) => Mode::Note(format!("{} failed: {e}", verb.word())),
                        };
                    }
                    Answer::No => mode = Mode::Browsing,
                    Answer::Ignore => {}
                }
                continue;
            }

            match decide(key, at, table.len()) {
                Step::Move(next) => {
                    at = Some(next);
                    mode = Mode::Browsing;
                }
                Step::Unselect => {
                    at = None;
                    mode = Mode::Browsing;
                }
                Step::Confirm(target) => mode = Mode::Confirming(target),
                Step::Refresh => poll_now = true,
                Step::Routes => {
                    if matches!(verb, Verb::Switch) {
                        let provider = at.and_then(|i| table.entries().get(i)).map(|e| e.provider);
                        mode = match accounts.edit_route(provider) {
                            Ok(Some(message)) => Mode::Note(message),
                            Ok(None) => Mode::Browsing,
                            Err(error) => Mode::Note(format!("route failed: {error}")),
                        };
                        painted.clear();
                        last_key = Instant::now();
                    }
                }
                Step::Quit => return Ok(Outcome::Quit),
                Step::Ignore => mode = Mode::Browsing,
            }
        } else if due(&mode, attempted.elapsed(), last_key.elapsed()) {
            poll_now = true;
        }

        if poll_now {
            mode = Mode::Note("refreshing…".to_string());
            let pending = frame(&table, at, &mode, style, polled, verb);
            paint(&pending)?;
            painted = pending;
            mode = repoll(accounts, &mut table, &mut at, &mut polled, &mut attempted);
        }
    }
}

/// Whether an unattended poll is due: never while a question is on screen,
/// never inside the grace period after a keypress, and not before the interval
/// has run out.
fn due(mode: &Mode, since_attempt: Duration, since_key: Duration) -> bool {
    !matches!(mode, Mode::Confirming(_)) && since_attempt >= AUTO_REFRESH && since_key >= IDLE_GRACE
}

/// Re-poll, keeping the last good reading when it fails.
///
/// `attempted` moves either way so a failing endpoint is retried on the usual
/// interval rather than on every tick; `polled` only moves on success, because
/// it is what the footer reports as the age of what is on screen.
fn repoll(
    accounts: &mut dyn Accounts,
    table: &mut Table,
    at: &mut Option<usize>,
    polled: &mut Instant,
    attempted: &mut Instant,
) -> Mode {
    *attempted = Instant::now();
    match accounts.poll() {
        Ok(next) => {
            *at = at.map(|at| at.min(next.len().saturating_sub(1)));
            *table = next;
            *polled = Instant::now();
            Mode::Browsing
        }
        Err(e) => Mode::Note(format!("refresh failed: {e}")),
    }
}

enum Step {
    Move(usize),
    /// Put the selection away, leaving no row under the cursor.
    Unselect,
    Confirm(usize),
    Refresh,
    Routes,
    Quit,
    Ignore,
}

enum Answer {
    Yes,
    No,
    Ignore,
}

/// Key handling as a pure decision, leaving the loop above about drawing.
///
/// Nothing selected is a resting state rather than an impossible one: it is
/// what `esc` backs out to, and with no row under the cursor `enter` has
/// nothing to act on. That is the point — the key that acts is one keystroke
/// from the key that moves, so there has to be a way to disarm it.
fn decide(key: KeyEvent, at: Option<usize>, len: usize) -> Step {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let last = len.saturating_sub(1);
    match key.code {
        KeyCode::Char('c') if ctrl => Step::Quit,
        KeyCode::Char('q') => Step::Quit,
        // One step out at a time: off the selection first, out of the picker
        // only once there is nothing left to back out of.
        KeyCode::Esc => match at {
            Some(_) => Step::Unselect,
            None => Step::Quit,
        },
        KeyCode::Enter => match at {
            Some(at) => Step::Confirm(at),
            None => Step::Ignore,
        },
        KeyCode::Char('r') => Step::Refresh,
        KeyCode::Char('m') => Step::Routes,
        // Moving is what brings the selection back, so putting it away never
        // strands the list. From nowhere, down lands on the first row and up
        // on the last, the way a menu opens from either end.
        KeyCode::Up | KeyCode::Char('k') => Step::Move(at.map_or(last, |at| at.saturating_sub(1))),
        KeyCode::Down | KeyCode::Char('j') => Step::Move(at.map_or(0, |at| (at + 1).min(last))),
        KeyCode::Home => Step::Move(0),
        KeyCode::End => Step::Move(last),
        _ => Step::Ignore,
    }
}

/// Answering a confirmation. Enter counts as yes because it is the key that
/// raised the question; anything unrecognised is ignored rather than guessed,
/// so a stray keypress cannot move an account.
fn answer(key: KeyEvent) -> Answer {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('c') if ctrl => Answer::No,
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => Answer::Yes,
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('q') | KeyCode::Esc => Answer::No,
        _ => Answer::Ignore,
    }
}

/// A bare message on an otherwise empty screen, for before there is a table to
/// put under it.
fn note(style: Style, text: &str) -> Result<()> {
    paint(&format!("\n  {}", style.dim(text)))
}

/// The whole screen as text, so the loop can tell whether anything moved before
/// spending a repaint on it.
fn frame(
    table: &Table,
    at: Option<usize>,
    mode: &Mode,
    style: Style,
    polled: Instant,
    verb: Verb,
) -> String {
    let mut lines = table.lines(at);
    lines.push(String::new());
    lines.push(footer(table, at, mode, style, polled, verb));
    lines.join("\n")
}

/// Clears line by line rather than clearing the screen up front, so a repaint
/// never shows an empty frame on the way through.
pub(crate) fn paint(frame: &str) -> Result<()> {
    let mut out = io::stdout().lock();
    queue!(out, cursor::MoveTo(0, 0))?;
    for line in frame.split('\n') {
        write!(out, "{line}")?;
        queue!(out, terminal::Clear(ClearType::UntilNewLine))?;
        write!(out, "\r\n")?;
    }
    queue!(out, terminal::Clear(ClearType::FromCursorDown))?;
    out.flush()?;
    Ok(())
}

fn footer(
    table: &Table,
    at: Option<usize>,
    mode: &Mode,
    style: Style,
    polled: Instant,
    verb: Verb,
) -> String {
    match mode {
        Mode::Browsing => {
            // Only offer the keys that do something: with nothing selected
            // there is no account to act on and nothing to put away.
            let mut keys = match at {
                Some(_) => {
                    format!("enter {}   esc unselect   r refresh   q quit", verb.word())
                }
                None => "r refresh   q quit".to_string(),
            };
            if matches!(verb, Verb::Switch) {
                keys.push_str("   m model routes");
            }
            style.dim(&format!("  up/down select   {keys}      updated {}", age(polled.elapsed())))
        }
        Mode::Note(text) => style.bold(&format!("  {text}")),
        Mode::Confirming(target) => {
            let Some(entry) = table.entries().get(*target) else { return String::new() };
            let question = format!("  {} [y/n]", render::question(entry, verb));
            match entry.restrictions().is_empty() {
                true => style.bold(&question),
                false => style.health(&question, Health::Critical),
            }
        }
    }
}

/// How old what is on screen is, in the fewest words that say it.
fn age(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    match seconds < 2 {
        true => "just now".to_string(),
        false => format!("{} ago", render::compact(seconds as i64)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn enter_asks_rather_than_switching_outright() {
        assert!(matches!(decide(press(KeyCode::Enter), Some(0), 3), Step::Confirm(0)));
    }

    #[test]
    fn movement_stays_inside_the_list() {
        assert!(matches!(decide(press(KeyCode::Up), Some(0), 3), Step::Move(0)));
        assert!(matches!(decide(press(KeyCode::Down), Some(2), 3), Step::Move(2)));
        assert!(matches!(decide(press(KeyCode::Down), Some(0), 3), Step::Move(1)));
        assert!(matches!(decide(press(KeyCode::End), Some(0), 3), Step::Move(2)));
    }

    #[test]
    fn movement_on_an_empty_list_lands_nowhere_rather_than_panicking() {
        assert!(matches!(decide(press(KeyCode::Down), Some(0), 0), Step::Move(0)));
        assert!(matches!(decide(press(KeyCode::End), Some(0), 0), Step::Move(0)));
    }

    #[test]
    fn escape_puts_the_selection_away_before_it_leaves() {
        assert!(matches!(decide(press(KeyCode::Esc), Some(1), 3), Step::Unselect));
        assert!(matches!(decide(press(KeyCode::Esc), None, 3), Step::Quit));
    }

    #[test]
    fn enter_does_nothing_with_no_row_under_it() {
        assert!(matches!(decide(press(KeyCode::Enter), None, 3), Step::Ignore));
    }

    #[test]
    fn moving_brings_the_selection_back_from_either_end() {
        assert!(matches!(decide(press(KeyCode::Down), None, 3), Step::Move(0)));
        assert!(matches!(decide(press(KeyCode::Up), None, 3), Step::Move(2)));
        assert!(matches!(decide(press(KeyCode::Char('j')), None, 3), Step::Move(0)));
    }

    #[test]
    fn refreshing_and_quitting_work_with_nothing_selected() {
        assert!(matches!(decide(press(KeyCode::Char('r')), None, 3), Step::Refresh));
        assert!(matches!(decide(press(KeyCode::Char('q')), None, 3), Step::Quit));
    }

    #[test]
    fn quitting_outright_answers_to_more_than_one_key() {
        assert!(matches!(decide(press(KeyCode::Char('q')), Some(0), 3), Step::Quit));
        let interrupt = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(decide(interrupt, Some(0), 3), Step::Quit));
    }

    #[test]
    fn a_confirmation_takes_yes_or_the_key_that_raised_it() {
        assert!(matches!(answer(press(KeyCode::Char('y'))), Answer::Yes));
        assert!(matches!(answer(press(KeyCode::Char('Y'))), Answer::Yes));
        assert!(matches!(answer(press(KeyCode::Enter)), Answer::Yes));
    }

    #[test]
    fn a_confirmation_takes_no_and_every_way_of_backing_out() {
        assert!(matches!(answer(press(KeyCode::Char('n'))), Answer::No));
        assert!(matches!(answer(press(KeyCode::Esc)), Answer::No));
        assert!(matches!(answer(press(KeyCode::Char('q'))), Answer::No));
        let interrupt = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(answer(interrupt), Answer::No));
    }

    #[test]
    fn an_unattended_poll_waits_for_the_interval() {
        assert!(!due(&Mode::Browsing, Duration::from_secs(30), Duration::from_secs(60)));
        assert!(due(&Mode::Browsing, AUTO_REFRESH, Duration::from_secs(60)));
    }

    #[test]
    fn an_unattended_poll_never_interrupts_a_question() {
        assert!(!due(&Mode::Confirming(0), AUTO_REFRESH, Duration::from_secs(60)));
    }

    #[test]
    fn an_unattended_poll_holds_off_right_after_a_keypress() {
        assert!(!due(&Mode::Browsing, AUTO_REFRESH, Duration::from_millis(200)));
        assert!(due(&Mode::Browsing, AUTO_REFRESH, IDLE_GRACE));
    }

    #[test]
    fn a_failed_poll_does_not_stop_the_next_one() {
        let noted = Mode::Note("refresh failed: offline".to_string());
        assert!(due(&noted, AUTO_REFRESH, Duration::from_secs(60)));
    }

    #[test]
    fn the_age_label_reads_plainly() {
        assert_eq!(age(Duration::from_secs(0)), "just now");
        assert_eq!(age(Duration::from_secs(1)), "just now");
        assert_eq!(age(Duration::from_secs(45)), "45s ago");
        assert_eq!(age(Duration::from_secs(90)), "1m ago");
    }

    #[test]
    fn a_stray_key_never_answers_a_confirmation() {
        for code in [KeyCode::Char('j'), KeyCode::Char('r'), KeyCode::Down, KeyCode::Char(' ')] {
            assert!(matches!(answer(press(code)), Answer::Ignore), "{code:?} should be ignored");
        }
    }
}
