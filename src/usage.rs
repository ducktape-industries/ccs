//! The usage cache: the last limits read for each account, written down.
//!
//! Every poll this tool makes already fetches an account's whole standing and
//! then throws it away when the table is gone. Something drawn far more often
//! than a table is worth — a status line repainting every few seconds — cannot
//! afford the round trip that produced it, so the reading is left where
//! anything can pick it up without asking the network.
//!
//! Limits are stored exactly as the endpoint reported them. Which windows exist,
//! and which models carry one, is the endpoint's to decide; a reader that
//! renders whatever it finds keeps a newly scoped model from needing a change
//! here.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions, Permissions};
use std::io::ErrorKind;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::fsx::write_atomic;
use crate::model::{Provider, UsageResponse};

/// Readings sit beside the stash, which is owner-only; what an account has
/// spent is nobody else's business either.
const FILE_MODE: u32 = 0o600;
const DIR_MODE: u32 = 0o700;

/// One account's standing as of the moment it was read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reading {
    /// When this was fetched, RFC 3339. A reader deciding whether to trust a
    /// reading has to be told its age, because nothing in the limits says.
    pub polled_at: String,
    #[serde(flatten)]
    pub usage: UsageResponse,
}

/// A usage endpoint rate limit, distinct from account quota exhaustion.
#[derive(Debug)]
pub struct RateLimited {
    pub retry_after: u64,
}

impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "usage endpoint returned 429; retry after {} seconds", self.retry_after)
    }
}
impl std::error::Error for RateLimited {}

#[derive(Default, Serialize, Deserialize)]
struct PollState {
    providers: BTreeMap<Provider, Cooldown>,
    attempts: BTreeMap<String, i64>,
}

#[derive(Default, Serialize, Deserialize)]
struct Cooldown {
    failures: u32,
    retry_at: i64,
}

pub struct Cache {
    dir: PathBuf,
}

impl Cache {
    pub fn open(root: &Path) -> Result<Self> {
        let dir = root.join("usage");
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        fs::set_permissions(&dir, Permissions::from_mode(DIR_MODE))
            .with_context(|| format!("securing {}", dir.display()))?;
        Ok(Self { dir })
    }

    /// One shared gate for app, watcher, picker and CLI. Holding an OS lock
    /// covers read/check/fetch/write; process exit releases it without stale-lock guessing.
    pub fn poll(
        &self,
        slug: &str,
        provider: Provider,
        fetch: impl FnOnce() -> Result<UsageResponse>,
    ) -> Result<UsageResponse> {
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(FILE_MODE)
            .open(self.dir.join(".poll.lock"))?;
        lock.lock()?;
        let now = Timestamp::now().as_second();
        if let Some(reading) = self.read(slug)?
            && let Ok(at) = reading.polled_at.parse::<Timestamp>()
            && (0..300).contains(&(now - at.as_second()))
        {
            return Ok(reading.usage);
        }
        let path = self.dir.join("poll-state.json");
        let mut state: PollState = match fs::read(&path) {
            Ok(raw) => serde_json::from_slice(&raw)?,
            Err(e) if e.kind() == ErrorKind::NotFound => PollState::default(),
            Err(e) => return Err(e.into()),
        };
        if let Some(cooldown) = state.providers.get(&provider)
            && cooldown.retry_at > now
        {
            anyhow::bail!(
                "{provider} usage refresh paused after 429; retry in {} seconds",
                cooldown.retry_at - now
            );
        }
        if state.attempts.get(slug).is_some_and(|at| *at > now) {
            anyhow::bail!("usage refresh was recently attempted; retry shortly");
        }
        state.attempts.insert(slug.into(), now + 60);
        write_atomic(&path, &serde_json::to_vec(&state)?, FILE_MODE)?;
        let result = fetch();
        match &result {
            Ok(usage) => {
                self.record(slug, usage)?;
                state.attempts.remove(slug);
                state.providers.remove(&provider);
            }
            Err(error) => {
                if let Some(limited) = error.downcast_ref::<RateLimited>() {
                    let cooldown = state.providers.entry(provider).or_default();
                    let delay = (600u64 * (1u64 << cooldown.failures.min(3)))
                        .min(3600)
                        .max(limited.retry_after)
                        .min(i64::MAX as u64 / 2);
                    cooldown.failures = cooldown.failures.saturating_add(1);
                    cooldown.retry_at = Timestamp::now().as_second().saturating_add(delay as i64);
                }
            }
        }
        write_atomic(&path, &serde_json::to_vec(&state)?, FILE_MODE)?;
        result
    }

    /// Write down what an account was last seen to have left.
    ///
    /// One file per account rather than one file of accounts, so two runs
    /// polling at once record their own readings instead of landing on each
    /// other's.
    pub fn record(&self, slug: &str, usage: &UsageResponse) -> Result<()> {
        let reading = Reading { polled_at: Timestamp::now().to_string(), usage: usage.clone() };
        let body = serde_json::to_vec_pretty(&reading).context("serialising a usage reading")?;
        write_atomic(&self.at(slug), &body, FILE_MODE)
    }

    /// What an account was last seen to have left, for a reader that cannot
    /// afford to ask. `None` when nothing has polled it yet.
    pub fn read(&self, slug: &str) -> Result<Option<Reading>> {
        let path = self.at(slug);
        let raw = match fs::read(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        serde_json::from_slice(&raw)
            .with_context(|| format!("parsing {}", path.display()))
            .map(Some)
    }

    /// Drop an account's reading. Having none is a resting state — an account
    /// can be forgotten before it was ever polled — so only a real failure to
    /// remove one is worth reporting.
    pub fn forget(&self, slug: &str) -> Result<()> {
        let path = self.at(slug);
        match fs::remove_file(&path) {
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            other => other.with_context(|| format!("removing {}", path.display())),
        }
    }

    fn at(&self, slug: &str) -> PathBuf {
        self.dir.join(format!("{slug}.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limit;

    /// A private root per test, so concurrently running tests never share a
    /// scratch path.
    struct Fixture {
        root: PathBuf,
        cache: Cache,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("ccs-usage-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).expect("root");
            Self { cache: Cache::open(&root).expect("cache"), root }
        }

        fn read(&self, slug: &str) -> Reading {
            let path = self.root.join("usage").join(format!("{slug}.json"));
            let raw = fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            serde_json::from_slice(&raw).expect("parses")
        }

        fn exists(&self, slug: &str) -> bool {
            self.root.join("usage").join(format!("{slug}.json")).exists()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn repeated_poll_reuses_reading_without_refreshing_its_timestamp() {
        let f = Fixture::new("reuse-poll");
        let first = f
            .cache
            .poll("a", crate::model::Provider::Claude, || Ok(vec![limit!("session", 7.0)].into()))
            .unwrap();
        let stamp = f.read("a").polled_at;
        let second = Cache::open(&f.root)
            .unwrap()
            .poll("a", crate::model::Provider::Claude, || panic!("duplicate request"))
            .unwrap();
        assert_eq!(first.limits[0].percent, second.limits[0].percent);
        assert_eq!(f.read("a").polled_at, stamp);
    }

    #[test]
    fn rate_limit_blocks_other_accounts_but_not_other_provider() {
        let f = Fixture::new("provider-cooldown");
        let result = f.cache.poll("a", crate::model::Provider::Claude, || {
            Err(RateLimited { retry_after: 1200 }.into())
        });
        assert!(result.is_err());
        let other = Cache::open(&f.root).unwrap();
        assert!(
            other
                .poll("b", crate::model::Provider::Claude, || panic!("provider still cooling down"))
                .is_err()
        );
        assert!(
            other.poll("c", crate::model::Provider::Codex, || Ok(UsageResponse::default())).is_ok()
        );
        let state: serde_json::Value =
            serde_json::from_slice(&fs::read(f.root.join("usage/poll-state.json")).unwrap())
                .unwrap();
        assert!(
            state["providers"]["claude"]["retry_at"].as_i64().unwrap()
                >= Timestamp::now().as_second() + 1199
        );
    }

    #[test]
    fn expired_readings_refresh_and_repeated_429_extends_backoff() {
        let f = Fixture::new("expired-poll");
        f.cache.record("a", &UsageResponse::default()).unwrap();
        let mut reading = f.read("a");
        reading.polled_at = "2020-01-01T00:00:00Z".into();
        fs::write(f.cache.at("a"), serde_json::to_vec(&reading).unwrap()).unwrap();
        assert!(
            f.cache
                .poll("a", crate::model::Provider::Claude, || Err(
                    RateLimited { retry_after: 0 }.into()
                ))
                .is_err()
        );
        assert_eq!(f.read("a").polled_at, reading.polled_at, "429 preserves last good reading");
        let path = f.root.join("usage/poll-state.json");
        let mut state: PollState = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        state.providers.get_mut(&crate::model::Provider::Claude).unwrap().retry_at = 0;
        state.attempts.clear();
        fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
        assert!(
            f.cache
                .poll("a", crate::model::Provider::Claude, || Err(
                    RateLimited { retry_after: 0 }.into()
                ))
                .is_err()
        );
        let state: PollState = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        let cooldown = &state.providers[&crate::model::Provider::Claude];
        assert_eq!(cooldown.failures, 2);
        assert!(cooldown.retry_at >= Timestamp::now().as_second() + 1199);
    }

    #[test]
    fn concurrent_cache_handles_only_fetch_once() {
        let f = Fixture::new("concurrent-poll");
        let calls = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let root = &f.root;
                let calls = &calls;
                scope.spawn(move || {
                    Cache::open(root)
                        .unwrap()
                        .poll("a", crate::model::Provider::Claude, || {
                            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            std::thread::sleep(std::time::Duration::from_millis(20));
                            Ok(UsageResponse::default())
                        })
                        .unwrap()
                });
            }
        });
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn a_recorded_reading_keeps_every_limit_as_it_was_reported() {
        let fixture = Fixture::new("whole");
        let limits = vec![
            limit!("session", 3.0, resets = "2026-01-01T00:00:00Z"),
            limit!("weekly_scoped", 100.0, model = "Fable", severity = "critical"),
        ];
        fixture.cache.record("you_at_example.com", &limits.into()).expect("records");

        let back = fixture.read("you_at_example.com");
        assert_eq!(back.usage.limits.len(), 2);
        assert_eq!(back.usage.limits[0].resets_at.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert_eq!(back.usage.limits[1].model_name(), Some("Fable"));
        assert_eq!(back.usage.limits[1].severity.as_deref(), Some("critical"));
    }

    #[test]
    fn availability_round_trips_but_is_not_carried_over_when_no_longer_reported() {
        let fixture = Fixture::new("availability");
        for at in [serde_json::json!("2026-09-07T03:00:00Z"), serde_json::json!(1788750000_i64)] {
            let raw = serde_json::json!({"limits": [], "model_usage": {"gpt-6-astra": {
                "available": false, "available_at": at, "credits_would_enable": true
            }}});
            let usage: UsageResponse = serde_json::from_value(raw.clone()).unwrap();
            fixture.cache.record("a", &usage).expect("records model gate");
            let back = fixture.cache.read("a").unwrap().unwrap();
            assert_eq!(serde_json::to_value(back.usage).unwrap(), raw);
            let file: serde_json::Value =
                serde_json::from_slice(&fs::read(fixture.cache.at("a")).unwrap()).unwrap();
            assert!(file.get("limits").is_some(), "existing cache field stays at the root");
            assert_eq!(file["model_usage"], raw["model_usage"]);
        }
        fixture.cache.record("a", &UsageResponse::default()).expect("fresh response");
        assert!(fixture.read("a").usage.model_usage.is_none());
    }

    #[test]
    fn an_older_cache_does_not_imply_any_model_is_available() {
        let reading: Reading = serde_json::from_value(serde_json::json!({
            "polled_at": "2026-09-07T00:00:00Z",
            "limits": [{"kind": "weekly_all", "percent": 12}]
        }))
        .expect("old cache");
        assert_eq!(reading.usage.limits[0].percent, 12.0);
        assert!(reading.usage.model_usage.is_none());
    }

    #[test]
    fn a_reading_is_stamped_with_a_time_a_reader_can_parse() {
        let fixture = Fixture::new("stamp");
        fixture.cache.record("a", &vec![limit!("session", 3.0)].into()).expect("records");

        let stamped = fixture.read("a").polled_at;
        let parsed: Timestamp = stamped.parse().unwrap_or_else(|e| panic!("{stamped}: {e}"));
        assert!((Timestamp::now().as_second() - parsed.as_second()).abs() < 60);
    }

    #[test]
    fn recording_again_replaces_the_reading_rather_than_adding_one() {
        let fixture = Fixture::new("replace");
        fixture.cache.record("a", &vec![limit!("session", 3.0)].into()).expect("records");
        fixture.cache.record("a", &vec![limit!("session", 40.0)].into()).expect("records again");

        let back = fixture.read("a");
        assert_eq!(back.usage.limits.len(), 1);
        assert_eq!(back.usage.limits[0].percent, 40.0);
    }

    #[test]
    fn one_accounts_reading_never_lands_on_anothers() {
        let fixture = Fixture::new("apart");
        fixture.cache.record("a", &vec![limit!("session", 3.0)].into()).expect("records a");
        fixture.cache.record("b", &vec![limit!("session", 90.0)].into()).expect("records b");

        assert_eq!(fixture.read("a").usage.limits[0].percent, 3.0);
        assert_eq!(fixture.read("b").usage.limits[0].percent, 90.0);
    }

    #[test]
    fn a_reading_is_owner_only_like_the_stash_beside_it() {
        let fixture = Fixture::new("mode");
        fixture.cache.record("a", &vec![limit!("session", 3.0)].into()).expect("records");

        let path = fixture.root.join("usage/a.json");
        let mode = fs::metadata(&path).expect("exists").permissions().mode();
        assert_eq!(mode & 0o777, FILE_MODE);
    }

    #[test]
    fn forgetting_an_account_takes_its_reading_with_it() {
        let fixture = Fixture::new("forget");
        fixture.cache.record("a", &vec![limit!("session", 3.0)].into()).expect("records");
        fixture.cache.forget("a").expect("forgets");
        assert!(!fixture.exists("a"));
    }

    #[test]
    fn a_reading_can_be_read_back_by_something_that_cannot_poll() {
        let fixture = Fixture::new("readback");
        fixture.cache.record("a", &vec![limit!("session", 42.0)].into()).expect("records");

        let back = fixture.cache.read("a").expect("reads").expect("a reading");
        assert_eq!(back.usage.limits[0].percent, 42.0);
        assert!(!back.polled_at.is_empty());
    }

    #[test]
    fn an_account_never_polled_has_no_reading_rather_than_a_broken_one() {
        let fixture = Fixture::new("unread");
        assert!(fixture.cache.read("never").expect("reads").is_none());
    }

    #[test]
    fn forgetting_an_account_that_was_never_polled_is_not_a_failure() {
        let fixture = Fixture::new("unpolled");
        assert!(fixture.cache.forget("never-seen").is_ok());
    }
}
