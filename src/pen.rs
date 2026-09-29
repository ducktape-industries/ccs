//! Pens: per-session configuration directories that differ from the real one in
//! two real files.
//!
//! Claude Code resolves everything it needs beneath `CLAUDE_CONFIG_DIR` — the
//! settings, the skills, the plugins, the global configuration file and the
//! credentials. A pen is a directory of symbolic links back to the real
//! configuration, with `.credentials.json` and the global configuration file
//! kept as the pen's own real files, so a session launched against it carries
//! the whole environment and only the account differs. A switch elsewhere
//! rewrites the real credentials and leaves the pen's standing.
//!
//! The global file is not a link because Claude Code writes its own account
//! identity into it, at startup and periodically while running. Two pens run
//! at once for two different accounts; a shared file would have whichever one
//! saves last decide what both sessions believe they are logged in as. See
//! `own_global`.

use std::fs::{self, Permissions};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::fsx::write_atomic;
use crate::lock;
use crate::model::Provider;

const DIR_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;

/// Records the configuration a pen was cut from. Named for this tool because it
/// sits in a directory Claude Code reads.
const MARKER: &str = ".ccs-pen.json";
const CODEX_MARKER: &str = ".ccs-codex-pen.json";

/// Claude Code's global configuration file, which it resolves beside the home
/// directory normally and inside `CLAUDE_CONFIG_DIR` when that is set.
pub const GLOBAL: &str = ".claude.json";

/// Names the mirror never links, because the pen owns them: the credentials are
/// the whole point of the pen, the stash would make the pen's copy a mirror of
/// itself, the write lock is held state rather than configuration, the marker
/// belongs to the pen alone, and the global configuration file is handled by
/// `own_global` — never by the generic per-name link, even when a file of that
/// name turns up inside the real configuration directory rather than beside it.
const PRIVATE: [&str; 5] = [".credentials.json", "ccs", ".storage-write.lock", MARKER, GLOBAL];

/// The top-level `.claude.json` key that names the logged-in account. Carrying
/// it into a pen's own copy would seed the pen with somebody else's identity,
/// and with a cached "already fetched" marker (`profileFetchedAt`, nested
/// inside this key) that keeps Claude Code from asking again for up to a day
/// — so a freshly split-off pen would go on claiming the account that
/// happened to write the shared file last. Dropping the key instead leaves it
/// absent, which Claude Code already treats as no cached profile and fills
/// back in from this pen's own credentials on its next start.
///
/// `userID` is deliberately not here: it is a random per-install analytics id
/// Claude Code generates locally when one is missing, not anything tied to
/// the logged-in account, so it is not part of the bug this strips and is
/// left alone.
const IDENTITY_KEYS: [&str; 1] = ["oauthAccount"];

/// Where the real configuration lives: the directory Claude Code reads, and the
/// global configuration file that sits outside it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Home {
    pub config: PathBuf,
    pub global: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
struct Marker {
    home: Home,
    account: String,
}

/// The configuration `config_dir` was cut from, when it is a pen.
///
/// Answering this is what keeps the stash reachable from inside a pen: the pen
/// deliberately does not mirror it, so a `ccs` that took the pen for the real
/// configuration would find no accounts at all.
pub fn home_of(config_dir: &Path) -> Option<Home> {
    let raw = fs::read(config_dir.join(MARKER)).ok()?;
    Some(serde_json::from_slice::<Marker>(&raw).ok()?.home)
}

/// The account currently installed in a Claude pen, which can change when a
/// pinned session switches accounts without moving to another directory.
pub fn account_of(config_dir: &Path) -> Option<String> {
    let raw = fs::read(config_dir.join(MARKER)).ok()?;
    Some(serde_json::from_slice::<Marker>(&raw).ok()?.account)
}

/// Re-mark a pen for a different account: the picker switching a pinned
/// session, `ccs repair`, and routine reconciliation all funnel through here.
///
/// Whenever the account actually changes, the pen's own global configuration
/// file is stripped of its identity first: it may already hold the *previous*
/// account's `oauthAccount`, `own_global` never revisits a file it already
/// owns, and Claude Code otherwise trusts a cached profile for up to a day
/// (see `IDENTITY_KEYS`). Stripping it here is the one place that covers
/// every way a pen's marked account can change, instead of chasing each
/// caller.
pub fn set_account(config_dir: &Path, account: &str) -> Result<()> {
    let path = config_dir.join(MARKER);
    let raw = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let mut marker: Marker = serde_json::from_slice(&raw)?;
    if marker.account != account {
        marker.account = account.to_string();
        forget_identity(config_dir)?;
    }
    write_atomic(&path, &serde_json::to_vec_pretty(&marker)?, FILE_MODE)
}

/// Strip the identity keys from a pen's own global configuration file, so the
/// next Claude Code start in it re-derives them from this pen's own (now
/// current) credentials instead of continuing to show whichever account it
/// was marked for before.
///
/// A no-op when the pen has no global file yet, or when it is still a symlink
/// to the shared one rather than a private copy: either way, the next
/// `prepare` seeds or migrates it, which already strips identity on the way in.
fn forget_identity(config_dir: &Path) -> Result<()> {
    let at = config_dir.join(GLOBAL);
    let bytes = match at.symlink_metadata() {
        Ok(meta) if !meta.file_type().is_symlink() => {
            fs::read(&at).with_context(|| format!("reading {}", at.display()))?
        }
        _ => return Ok(()),
    };
    match strip_identity(&bytes) {
        Some(seeded) => write_atomic(&at, &seeded, FILE_MODE),
        None => Ok(()),
    }
}

/// Where `slug`'s pen is kept, whether or not one has been built there.
///
/// Answering this without building anything is what lets the credentials a pen
/// holds be read by a run that is not launching a session into it.
pub fn at(root: &Path, slug: &str) -> PathBuf {
    root.join("pens").join(slug)
}

/// Build, or bring up to date, the pen belonging to `slug`.
pub fn prepare(home: &Home, root: &Path, slug: &str) -> Result<PathBuf> {
    let pen = at(root, slug);
    fs::create_dir_all(&pen).with_context(|| format!("creating {}", pen.display()))?;
    fs::set_permissions(&pen, Permissions::from_mode(DIR_MODE))
        .with_context(|| format!("securing {}", pen.display()))?;
    let _guard = lock::acquire(&pen)?;
    if pen.join(MARKER).exists() && account_of(&pen).is_none() {
        bail!("{} has an unreadable pen marker; refusing to replace it", pen.display());
    }
    if let Some(current) = account_of(&pen)
        && current != slug
    {
        bail!(
            "{} is still assigned to {current}; refusing to reassign a pen used by another session",
            pen.display()
        );
    }
    let marker = Marker { home: home.clone(), account: slug.to_string() };
    let body = serde_json::to_vec_pretty(&marker).context("serialising the pen marker")?;
    write_atomic(&pen.join(MARKER), &body, FILE_MODE)?;

    mirror(&pen, home)?;
    Ok(pen)
}

/// Codex pins share configuration; runtime files are never mirrored.
/// SQLite files and their journals must
/// stay together; mirroring a changing home entry by entry cannot ensure that.
#[derive(Serialize, Deserialize)]
struct CodexOrigin {
    home: PathBuf,
}

pub fn codex_home_of(config: &Path) -> Result<Option<PathBuf>> {
    let path = config.join(CODEX_MARKER);
    let raw = match fs::read(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let origin: CodexOrigin =
        serde_json::from_slice(&raw).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(origin.home))
}

pub fn prepare_codex(home: &Path, root: &Path, slug: &str) -> Result<PathBuf> {
    let source = codex_home_of(home)?.unwrap_or_else(|| home.to_path_buf());
    fs::create_dir_all(&source)?;
    let source = fs::canonicalize(source)?;
    let pen = at(root, slug);
    fs::create_dir_all(&pen)?;
    fs::set_permissions(&pen, Permissions::from_mode(DIR_MODE))?;
    let origin = CodexOrigin { home: source.clone() };
    write_atomic(&pen.join(CODEX_MARKER), &serde_json::to_vec_pretty(&origin)?, FILE_MODE)?;
    prune(&pen);
    for entry in fs::read_dir(&source)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        let shared_configuration = matches!(
            name,
            "config.toml" | "AGENTS.md" | "skills" | "plugins" | "rules" | "prompts"
        ) || name.ends_with(".config.toml");
        if shared_configuration {
            link(&path, &pen.join(name))?;
        }
    }
    Ok(pen)
}

/// Remove credentials. Codex pins retain their private conversation history.
pub fn discard(root: &Path, slug: &str) -> Result<()> {
    let pen = at(root, slug);
    if codex_home_of(&pen)?.is_some() {
        return match fs::remove_file(pen.join("auth.json")) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            result => result.with_context(|| format!("removing {}'s credentials", pen.display())),
        };
    }
    match pen.exists() {
        true => fs::remove_dir_all(&pen).with_context(|| format!("removing {}", pen.display())),
        false => Ok(()),
    }
}

/// Hand the process over to Claude Code inside `pen`.
///
/// The image is replaced rather than a child being waited on, so nothing of
/// this tool outlives the launch and the terminal, the signals and the exit
/// status all belong to the session.
///
/// Only ever returns on failure to launch.
pub fn launch(pen: &Path, provider: Provider, binary: &str, args: &[String]) -> Result<()> {
    let error = launch_command(pen, provider, binary, args).exec();
    let override_env = match provider {
        Provider::Claude => "CCS_CLAUDE_BINARY",
        Provider::Codex => "CCS_CODEX_BINARY",
    };
    Err(error)
        .with_context(|| format!("running `{binary}`; set {override_env} if it is not on PATH"))
}

fn launch_command(pen: &Path, provider: Provider, binary: &str, args: &[String]) -> Command {
    let mut command = Command::new(binary);
    match provider {
        Provider::Claude => {
            command.env("CLAUDE_CONFIG_DIR", pen);
        }
        Provider::Codex => {
            command.env("CODEX_HOME", pen);
            command.args(["-c", "cli_auth_credentials_store=\"file\""]);
        }
    }
    command.args(args);
    command
}

/// Point every name in the real configuration at itself from inside the pen.
///
/// Only missing names are linked: whatever the pen already holds stays, so a
/// link Claude Code has replaced with a file of its own remains the pen's.
fn mirror(pen: &Path, home: &Home) -> Result<()> {
    prune(pen);

    let entries =
        fs::read_dir(&home.config).with_context(|| format!("reading {}", home.config.display()))?;
    for entry in entries {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        if PRIVATE.contains(&name) || is_staging_artifact(name) {
            continue;
        }
        link(&path, &pen.join(name))?;
    }

    // The global configuration file lives outside the configuration directory
    // and moves inside `CLAUDE_CONFIG_DIR` when that is set, so a pen that did
    // not carry it would start a session with no onboarding and no project it
    // has ever been trusted in. It is copied rather than linked: see
    // `own_global`.
    own_global(pen, &home.global)
}

fn link(target: &Path, at: &Path) -> Result<()> {
    if at.symlink_metadata().is_ok() {
        return Ok(());
    }
    std::os::unix::fs::symlink(target, at)
        .with_context(|| format!("linking {} to {}", at.display(), target.display()))
}

/// `write_atomic`'s own staging name for a replacement. `write_atomic` now
/// cleans this up itself on a failed rename, so nothing new should appear
/// here; the check stays to keep the handful of copies leaked by builds
/// before that fix from being symlinked into a pen that has never seen one.
/// The mirror creates a symlink here, not a copy — inside the same 0700
/// `~/.claude` tree every pen already sits under, so this adds no reach a
/// process confined to one pen did not already have; the harm it heads off is
/// clutter and confusion, a pen full of names that belong to nothing it did.
fn is_staging_artifact(name: &str) -> bool {
    name.contains(".ccs-") && name.ends_with(".tmp")
}

/// Give the pen its own copy of the global configuration file instead of a
/// link to the shared one. Three cases:
///
/// - **No file yet** (a pen `prepare` is building for the first time): seed
///   it from `global`, the shared file, if one exists.
/// - **Still a symlink**: only a pen built by a version of `ccs` before this
///   existed reaches this branch — the mirror loop above never creates this
///   link any more, since `GLOBAL` is in `PRIVATE`. Migrated by reading
///   *through* the link, so whatever the pen has been accumulating (trust
///   decisions, onboarding state) survives the switch to a private copy.
/// - **Already a real file**: left untouched. Re-seeding it on every
///   `prepare` would erase the identity Claude Code, or `set_account`'s
///   `forget_identity`, has since written into it, restarting the same race
///   scoped to just this one pen.
///
/// In the first two cases the identity key is dropped either way, since
/// keeping it would carry forward whichever account last wrote the source.
fn own_global(pen: &Path, global: &Path) -> Result<()> {
    let at = pen.join(GLOBAL);
    let bytes = match at.symlink_metadata() {
        Ok(meta) if meta.file_type().is_symlink() => {
            fs::read(&at).with_context(|| format!("reading {}", at.display()))?
        }
        Ok(_) => return Ok(()), // already the pen's own file
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => match fs::read(global) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", global.display()));
            }
        },
        Err(error) => return Err(error).with_context(|| format!("checking {}", at.display())),
    };
    match strip_identity(&bytes) {
        Some(seeded) => write_atomic(&at, &seeded, FILE_MODE),
        // Not a shape this can safely check for identity keys: leave the pen
        // without a copy sooner than freeze a payload that might still hold
        // one. The next `prepare` tries again from the same source.
        None => Ok(()),
    }
}

/// Drop the identity key from a `.claude.json` payload, or refuse the payload
/// outright.
///
/// `None` covers invalid JSON and valid JSON that is not an object: either
/// way there is no reliable way to check whether it holds `oauthAccount`, so
/// copying it through as-is would risk freezing another account's identity
/// into a pen's file with no signal anything was wrong. Callers treat `None`
/// as "nothing usable to seed from this time" rather than installing it.
fn strip_identity(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let map = value.as_object_mut()?;
    for key in IDENTITY_KEYS {
        map.remove(key);
    }
    serde_json::to_vec_pretty(&value).ok()
}

/// Drop links whose target has gone, so a name the real configuration lost and
/// regained is linked afresh rather than resolving to nothing.
fn prune(pen: &Path) {
    let Ok(entries) = fs::read_dir(pen) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = path.symlink_metadata() else { continue };
        if meta.is_symlink() && !path.exists() {
            let _ = fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A private root per test, so concurrently running tests never share a path.
    struct Fixture {
        root: PathBuf,
        home: Home,
    }

    impl Fixture {
        /// A real configuration holding one of everything a pen has to reason
        /// about: an ordinary file, a directory, a dotfile, the names inside
        /// `config` the pen owns rather than mirrors, and a global
        /// configuration file seeded with identity keys a fresh pen must drop.
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("ccs-pen-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&root);

            let config = root.join("claude");
            fs::create_dir_all(config.join("skills")).expect("skills");
            fs::create_dir_all(config.join("ccs")).expect("stash");
            fs::create_dir_all(config.join(".storage-write.lock")).expect("lock");
            fs::write(config.join("settings.json"), b"{}").expect("settings");
            fs::write(config.join(".last-cleanup"), b"").expect("dotfile");
            fs::write(config.join(".credentials.json"), b"{}").expect("credentials");

            let global = root.join(GLOBAL);
            fs::write(
                &global,
                br#"{"oauthAccount":{"emailAddress":"shared@example.com"},"userID":"shared-uid","projects":{"/tmp/work":{"trusted":true}}}"#,
            )
            .expect("global");

            Self { root, home: Home { config, global } }
        }

        fn prepare(&self) -> PathBuf {
            prepare(&self.home, &self.root, "someone_at_example.com").expect("prepare")
        }
    }

    fn json(path: &Path) -> serde_json::Value {
        serde_json::from_slice(&fs::read(path).expect("read")).expect("parse")
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn links_to(pen: &Path, name: &str) -> Option<PathBuf> {
        fs::read_link(pen.join(name)).ok()
    }

    #[test]
    fn a_codex_pin_shares_configuration_and_keeps_runtime_state_private() {
        let fixture = Fixture::new("codex-mirror");
        let home = fixture.root.join("codex");
        fs::create_dir_all(home.join("skills")).unwrap();
        for name in [
            "config.toml",
            "AGENTS.md",
            "work.config.toml",
            "auth.json",
            "state_5.sqlite",
            "state_5.sqlite-wal",
            "history.jsonl",
        ] {
            fs::write(home.join(name), "source").unwrap();
        }
        let pen = prepare_codex(&home, &fixture.root, "codex-work").unwrap();
        // The pen links into the resolved home; on macOS the temp directory
        // sits behind the `/var` -> `/private/var` link.
        let home = fs::canonicalize(home).unwrap();
        for name in ["config.toml", "AGENTS.md", "skills", "work.config.toml"] {
            assert_eq!(links_to(&pen, name), Some(home.join(name)));
        }
        for name in ["auth.json", "state_5.sqlite", "state_5.sqlite-wal", "history.jsonl"] {
            assert!(!pen.join(name).exists(), "{name}");
        }
        fs::write(home.join("config.toml"), "updated").unwrap();
        assert_eq!(fs::read_to_string(pen.join("config.toml")).unwrap(), "updated");
        let second = prepare_codex(&pen, &fixture.root, "codex-other").unwrap();
        assert_eq!(codex_home_of(&second).unwrap(), Some(home));
        assert_eq!(fs::metadata(pen).unwrap().permissions().mode() & 0o777, DIR_MODE);
    }

    #[test]
    fn forgetting_a_codex_pin_removes_credentials_and_preserves_conversations() {
        let fixture = Fixture::new("codex-discard");
        let home = fixture.root.join("codex");
        let pen = prepare_codex(&home, &fixture.root, "codex-work").unwrap();
        fs::write(pen.join("auth.json"), "credentials").unwrap();
        fs::write(pen.join("history.jsonl"), "conversation").unwrap();
        discard(&fixture.root, "codex-work").unwrap();
        assert!(!pen.join("auth.json").exists());
        assert_eq!(fs::read_to_string(pen.join("history.jsonl")).unwrap(), "conversation");
        discard(&fixture.root, "codex-work").unwrap();
    }

    #[test]
    fn a_broken_codex_marker_never_turns_history_into_a_disposable_claude_pen() {
        let fixture = Fixture::new("codex-broken-marker");
        let pen = prepare_codex(&fixture.root.join("codex"), &fixture.root, "codex-work").unwrap();
        fs::write(pen.join(CODEX_MARKER), "broken").unwrap();
        fs::write(pen.join("history.jsonl"), "conversation").unwrap();
        assert!(discard(&fixture.root, "codex-work").is_err());
        assert_eq!(fs::read_to_string(pen.join("history.jsonl")).unwrap(), "conversation");
    }

    #[test]
    fn a_codex_launch_selects_its_home_and_file_credentials_and_forwards_args() {
        let args = vec!["resume".into(), "--last".into()];
        let command = launch_command(Path::new("/pin"), Provider::Codex, "codex", &args);
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["-c", "cli_auth_credentials_store=\"file\"", "resume", "--last"]
        );
        assert!(command.get_envs().any(
            |(name, value)| name == "CODEX_HOME" && value == Some(std::ffi::OsStr::new("/pin"))
        ));
        assert!(!command.get_envs().any(|(name, _)| name == "CLAUDE_CONFIG_DIR"));
    }

    #[test]
    fn a_pen_links_every_name_the_real_configuration_has() {
        let fixture = Fixture::new("mirror");
        let pen = fixture.prepare();

        for name in ["skills", "settings.json", ".last-cleanup"] {
            assert_eq!(
                links_to(&pen, name).as_deref(),
                Some(fixture.home.config.join(name).as_path()),
                "{name} should be linked into the pen"
            );
        }
    }

    #[test]
    fn a_pen_owns_its_credentials_rather_than_borrowing_them() {
        let pen = Fixture::new("private").prepare();
        for name in [".credentials.json", "ccs", ".storage-write.lock"] {
            assert!(!pen.join(name).exists(), "{name} must not be mirrored into a pen");
        }
    }

    #[test]
    fn a_pen_owns_the_global_configuration_file_rather_than_sharing_it() {
        let fixture = Fixture::new("global");
        let pen = fixture.prepare();

        assert_eq!(links_to(&pen, GLOBAL), None, "the global file must not be a symlink");
        let value = json(&pen.join(GLOBAL));
        assert_eq!(value.get("oauthAccount"), None, "identity must not be seeded into a new pen");
        assert_eq!(
            value["userID"], "shared-uid",
            "userID is a per-install analytics id, not account identity; it is not stripped"
        );
        assert_eq!(
            value["projects"]["/tmp/work"]["trusted"], true,
            "shared state should carry over"
        );
    }

    #[test]
    fn a_name_that_collides_with_the_global_file_inside_the_real_directory_is_not_mirrored() {
        let fixture = Fixture::new("global-collision");
        // A file the real config directory happens to hold under the same name
        // the global configuration file has, distinct from `home.global` itself
        // — reproducing the drift a broken pen was found with in production.
        fs::write(
            fixture.home.config.join(GLOBAL),
            br#"{"oauthAccount":{"emailAddress":"wrong@example.com"}}"#,
        )
        .expect("stray file");
        let pen = fixture.prepare();

        assert_eq!(links_to(&pen, GLOBAL), None);
        let value = json(&pen.join(GLOBAL));
        assert_eq!(value.get("oauthAccount"), None);
        assert_eq!(
            value["projects"]["/tmp/work"]["trusted"], true,
            "seeded from home.global, not the stray file"
        );
    }

    #[test]
    fn a_pens_global_file_already_owned_is_never_reseeded() {
        let fixture = Fixture::new("global-owned");
        let pen = fixture.prepare();

        // Claude Code has since logged this pen in and written its own
        // identity, plus trust for a project no other pen has seen.
        fs::write(
            pen.join(GLOBAL),
            br#"{"oauthAccount":{"emailAddress":"someone@example.com"},"projects":{"/tmp/mine":{"trusted":true}}}"#,
        )
        .expect("simulate claude code");

        fixture.prepare();

        let value = json(&pen.join(GLOBAL));
        assert_eq!(value["oauthAccount"]["emailAddress"], "someone@example.com");
        assert_eq!(value["projects"]["/tmp/mine"]["trusted"], true);
    }

    #[test]
    fn a_symlinked_global_file_from_before_this_fix_is_migrated_to_a_private_copy() {
        let fixture = Fixture::new("global-migrate");
        let pen = at(&fixture.root, "someone_at_example.com");
        fs::create_dir_all(&pen).expect("pen dir");

        // What an old pen looked like: `.claude.json` linked straight at
        // whatever the shared file was, already carrying another account's
        // identity and this pen's own accumulated trust.
        let shared = fixture.root.join("shared.claude.json");
        fs::write(
            &shared,
            br#"{"oauthAccount":{"emailAddress":"other@example.com"},"projects":{"/tmp/accumulated":{"trusted":true}}}"#,
        )
        .expect("legacy shared file");
        std::os::unix::fs::symlink(&shared, pen.join(GLOBAL)).expect("legacy symlink");

        fixture.prepare();

        assert_eq!(links_to(&pen, GLOBAL), None, "the pen must own a private copy now");
        let value = json(&pen.join(GLOBAL));
        assert_eq!(value.get("oauthAccount"), None, "the old identity must not carry over");
        assert_eq!(
            value["projects"]["/tmp/accumulated"]["trusted"], true,
            "what this pen accumulated through the old link must survive the migration"
        );
    }

    #[test]
    fn a_global_file_that_is_not_a_json_object_is_never_seeded() {
        let fixture = Fixture::new("global-invalid");
        // Truncated JSON: enough to look like an oauthAccount block, not
        // enough to parse, so there is no reliable way to check it for the
        // key that must not carry over.
        fs::write(
            &fixture.home.global,
            br#"{"oauthAccount":{"emailAddress":"other@example.com"},"projects":{}"#,
        )
        .expect("invalid json");

        let pen = fixture.prepare();

        assert!(!pen.join(GLOBAL).exists(), "an unparseable payload must not be frozen into a pen");
    }

    #[test]
    fn a_global_file_that_is_valid_json_but_not_an_object_is_never_seeded() {
        let fixture = Fixture::new("global-not-object");
        fs::write(&fixture.home.global, b"[1,2,3]").expect("json array");

        let pen = fixture.prepare();

        assert!(!pen.join(GLOBAL).exists());
    }

    #[test]
    fn switching_a_pens_account_in_place_forgets_the_old_identity() {
        let fixture = Fixture::new("switch-in-place");
        let pen = fixture.prepare();

        // Claude Code logged this pen in as the account it was pinned to.
        fs::write(
            pen.join(GLOBAL),
            br#"{"oauthAccount":{"emailAddress":"old@example.com"},"projects":{"/tmp/work":{"trusted":true}}}"#,
        )
        .expect("simulate claude code");

        // A session running inside the pen switches accounts without moving
        // to another pen (cmd.rs's switch_to, run inside a pin).
        set_account(&pen, "new_at_example.com").expect("switch account");

        assert_eq!(account_of(&pen).as_deref(), Some("new_at_example.com"));
        let value = json(&pen.join(GLOBAL));
        assert_eq!(value.get("oauthAccount"), None, "the previous account's identity must be gone");
        assert_eq!(
            value["projects"]["/tmp/work"]["trusted"], true,
            "everything else in the file must survive the switch"
        );
    }

    #[test]
    fn setting_the_same_account_again_does_not_touch_the_global_file() {
        let fixture = Fixture::new("switch-noop");
        let pen = fixture.prepare();
        fs::write(pen.join(GLOBAL), br#"{"oauthAccount":{"emailAddress":"me@example.com"}}"#)
            .expect("simulate claude code");

        set_account(&pen, "someone_at_example.com").expect("same account");

        let value = json(&pen.join(GLOBAL));
        assert_eq!(value["oauthAccount"]["emailAddress"], "me@example.com");
    }

    #[test]
    fn switching_a_symlinked_pens_account_leaves_the_link_for_the_next_prepare() {
        let fixture = Fixture::new("switch-symlinked");
        let pen = at(&fixture.root, "someone_at_example.com");
        fs::create_dir_all(&pen).expect("pen dir");
        let marker =
            super::Marker { home: fixture.home.clone(), account: "someone_at_example.com".into() };
        fs::write(pen.join(super::MARKER), serde_json::to_vec(&marker).unwrap()).expect("marker");
        std::os::unix::fs::symlink(&fixture.home.global, pen.join(GLOBAL)).expect("legacy symlink");

        set_account(&pen, "new_at_example.com").expect("switch account");

        assert_eq!(
            links_to(&pen, GLOBAL).as_deref(),
            Some(fixture.home.global.as_path()),
            "forget_identity must not touch a file it does not yet own"
        );
    }

    #[test]
    fn a_stray_write_atomic_tmp_file_is_never_mirrored_into_a_pen() {
        let fixture = Fixture::new("staging-artifact");
        fs::write(fixture.home.config.join(".credentials.json.ccs-4242.tmp"), b"leaked-token")
            .expect("staging artifact");

        let pen = fixture.prepare();
        assert!(!pen.join(".credentials.json.ccs-4242.tmp").exists());
    }

    #[test]
    fn a_pen_says_which_configuration_it_was_cut_from() {
        let fixture = Fixture::new("marker");
        let pen = fixture.prepare();
        assert_eq!(home_of(&pen), Some(fixture.home.clone()));
    }

    #[test]
    fn a_directory_that_is_not_a_pen_claims_no_home() {
        let fixture = Fixture::new("unmarked");
        assert_eq!(home_of(&fixture.home.config), None);
    }

    #[test]
    fn preparing_a_pen_a_second_time_leaves_it_as_it_was() {
        let fixture = Fixture::new("idempotent");
        let pen = fixture.prepare();
        let before = names(&pen);
        assert_eq!(fixture.prepare(), pen);
        assert_eq!(names(&pen), before);
    }

    #[test]
    fn preparing_a_switched_pen_does_not_steal_the_running_session() {
        let fixture = Fixture::new("switched-owner");
        let pen = fixture.prepare();
        set_account(&pen, "robin").expect("switch pen");

        assert!(prepare(&fixture.home, &fixture.root, "someone_at_example.com").is_err());
        assert_eq!(account_of(&pen).as_deref(), Some("robin"));
    }

    #[test]
    fn a_file_the_pen_has_made_its_own_is_not_replaced_by_a_link() {
        let fixture = Fixture::new("owned");
        let pen = fixture.prepare();

        fs::remove_file(pen.join("settings.json")).expect("unlink");
        fs::write(pen.join("settings.json"), b"{\"mine\":true}").expect("own it");
        fixture.prepare();

        assert_eq!(links_to(&pen, "settings.json"), None, "the pen's own file should survive");
        assert_eq!(fs::read(pen.join("settings.json")).expect("read"), b"{\"mine\":true}");
    }

    #[test]
    fn a_link_whose_target_has_gone_is_dropped() {
        let fixture = Fixture::new("dangling");
        let pen = fixture.prepare();

        fs::remove_file(fixture.home.config.join("settings.json")).expect("remove target");
        fixture.prepare();
        assert!(pen.join("settings.json").symlink_metadata().is_err(), "a dead link should go");
    }

    #[test]
    fn a_name_that_comes_back_is_linked_afresh() {
        let fixture = Fixture::new("returning");
        let pen = fixture.prepare();
        let target = fixture.home.config.join("settings.json");

        fs::remove_file(&target).expect("remove target");
        fixture.prepare();
        fs::write(&target, b"{}").expect("bring it back");
        fixture.prepare();

        assert_eq!(links_to(&pen, "settings.json").as_deref(), Some(target.as_path()));
    }

    #[test]
    fn discarding_takes_the_credentials_with_it() {
        let fixture = Fixture::new("discard");
        let pen = fixture.prepare();
        fs::write(pen.join(".credentials.json"), b"{}").expect("credentials");

        discard(&fixture.root, "someone_at_example.com").expect("discard");
        assert!(!pen.exists());
    }

    #[test]
    fn discarding_a_pen_that_was_never_built_is_not_an_error() {
        let fixture = Fixture::new("absent");
        assert!(discard(&fixture.root, "nobody").is_ok());
    }

    fn names(pen: &Path) -> Vec<String> {
        let mut out: Vec<String> = fs::read_dir(pen)
            .expect("read pen")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        out.sort();
        out
    }
}
