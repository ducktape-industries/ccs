//! Command implementations: the orchestration between the CLI surface and the
//! credential store, the stash, and the API.
//!
//! Collaborators arrive through `Ctx` rather than being reached for, so each
//! command stays testable against substitutes.

use std::io::{self, IsTerminal, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use jiff::Timestamp;
use serde::Serialize;

use crate::api::{Api, refreshed_oauth};
use crate::codex;
use crate::codex::Creds as _;
use crate::creds::{self, Backend, CredStore};
use crate::lock;
use crate::login;
use crate::model::{
    Account, CredsFile, Limit, ModelAvailability, Oauth, Provider, Stashed, UsageResponse,
    plan_label,
};
use crate::notify;
use crate::pen;
use crate::picker::{self, Act, Outcome};
use crate::render::{self, Entry, Style, Table, Verb};
use crate::serve::{self, Grant};
use crate::stash::{self, Stash};
use crate::usage;
use crate::watch;

pub struct Ctx<'a> {
    pub creds: &'a dyn CredStore,
    /// Where credentials are kept on this machine, for the stores this reaches
    /// for itself: a pen's, and the one a login is captured from.
    pub backend: Backend,
    pub stash: &'a Stash,
    /// Where a poll's findings are left for readers that cannot make one.
    pub usage: &'a usage::Cache,
    pub api: &'a Api,
    /// Codex's live slot and the client that speaks to its endpoints.
    pub codex: &'a dyn codex::Creds,
    pub codex_api: &'a codex::Client,
    /// The real configuration: where the stash lives and what pens are cut
    /// from. The same directory `creds` reads, unless this process is itself
    /// in a pen.
    pub home: &'a pen::Home,
}

impl Ctx<'_> {
    fn apis(&self) -> Apis<'_> {
        Apis { claude: self.api, codex: self.codex_api }
    }
}

/// The clients each provider is spoken to with. Copyable so a probe can run
/// on a thread of its own without the rest of the context going with it.
#[derive(Clone, Copy)]
struct Apis<'a> {
    claude: &'a Api,
    codex: &'a codex::Client,
}

/// Which stashed account each provider's live slot holds, when it is known.
#[derive(Debug, Clone, Default, PartialEq)]
struct Live {
    claude: Option<String>,
    codex: Option<String>,
}

impl Live {
    fn of(&self, provider: Provider) -> Option<&str> {
        match provider {
            Provider::Claude => self.claude.as_deref(),
            Provider::Codex => self.codex.as_deref(),
        }
    }

    fn set(&mut self, provider: Provider, slug: Option<String>) {
        match provider {
            Provider::Claude => self.claude = slug,
            Provider::Codex => self.codex = slug,
        }
    }

    /// Whether `entry` is the account installed in its provider's slot.
    fn holds(&self, entry: &Stashed) -> bool {
        self.of(entry.account.provider) == Some(entry.slug.as_str())
    }
}

/// What each provider's live slot holds right now, read off disk.
struct Slots {
    claude: Option<Oauth>,
    codex: Option<Oauth>,
}

impl Slots {
    fn of(&self, provider: Provider) -> Option<&Oauth> {
        match provider {
            Provider::Claude => self.claude.as_ref(),
            Provider::Codex => self.codex.as_ref(),
        }
    }
}

fn slot(ctx: &Ctx, provider: Provider) -> Result<Option<Oauth>> {
    Ok(match provider {
        Provider::Claude => ctx.creds.read()?.map(|f| f.oauth),
        Provider::Codex => ctx.codex.read()?.and_then(|f| f.oauth()),
    })
}

fn slots(ctx: &Ctx) -> Result<Slots> {
    Ok(Slots { claude: slot(ctx, Provider::Claude)?, codex: slot(ctx, Provider::Codex)? })
}

// ── probing ─────────────────────────────────────────────────────────────────

/// The outcome of asking one account how it is doing.
struct Probe {
    slug: String,
    /// Present when the access token had to be refreshed. The caller must
    /// persist it: the refresh may have rotated the refresh token, and a
    /// rotated token that is not written down costs an interactive re-login.
    refreshed: Option<Oauth>,
    usage: Result<UsageResponse, String>,
}

/// Ask one account for its limits, refreshing its access token first if the
/// token is spent. Recent readings and rate-limit cooldowns skip the network;
/// refreshed credentials are still persisted by the caller.
fn probe(ctx: &Ctx, entry: &Stashed) -> Probe {
    let mut refreshed = None;
    let usage = ctx
        .usage
        .poll(&entry.slug, entry.account.provider, || {
            refreshed = freshen(ctx.apis(), entry.account.provider, &entry.account.oauth)?;
            let oauth = refreshed.as_ref().unwrap_or(&entry.account.oauth);
            read_usage(ctx.apis(), entry.account.provider, oauth)
        })
        .map_err(|e| describe(&e));
    Probe { slug: entry.slug.clone(), refreshed, usage }
}

/// What an account has left, asked of its provider.
fn read_usage(apis: Apis, provider: Provider, oauth: &Oauth) -> Result<UsageResponse> {
    match provider {
        Provider::Claude => apis.claude.usage(&oauth.access_token),
        Provider::Codex => {
            let account = oauth.account_id().context("a Codex login that names no account")?;
            Ok(apis.codex.usage(&oauth.access_token, account)?.into())
        }
    }
}

fn probe_all(ctx: &Ctx, accounts: &[Stashed]) -> Vec<Probe> {
    // Sequential so one provider's 429 stops its remaining accounts immediately.
    accounts.iter().map(|entry| probe(ctx, entry)).collect()
}

/// Give back a refreshed credential blob when the current one is spent.
fn freshen(apis: Apis, provider: Provider, oauth: &Oauth) -> Result<Option<Oauth>> {
    if !oauth.needs_refresh() {
        return Ok(None);
    }
    Ok(Some(match provider {
        Provider::Claude => {
            refreshed_oauth(oauth, &apis.claude.refresh(&oauth.refresh_token, &oauth.scopes)?)
        }
        Provider::Codex => codex::refreshed(oauth, &apis.codex.refresh(&oauth.refresh_token)?),
    }))
}

// ── credential copies ───────────────────────────────────────────────────────
//
// One account's credentials are kept in more than one place: its stash entry,
// the live credentials while it is the installed account, and its pen while a
// session is confined to it. Claude Code refreshes whichever of them it is
// pointed at, and a refresh spends the token it presents — so a copy this tool
// was not the last writer of holds a token the server will not honour again.
// Keeping an account's copies in step is what stops that being discovered as a
// failed refresh.

/// Every copy of one account's credentials there is to read.
///
/// `installed` is what its provider's live slot holds, and belongs here only
/// when that is this account. Pins contribute their provider's credentials.
fn copies(ctx: &Ctx, entry: &Stashed, installed: Option<&Oauth>) -> Result<Vec<Oauth>> {
    let mut held = vec![entry.account.oauth.clone()];
    held.extend(installed.cloned());
    match entry.account.provider {
        Provider::Claude => {
            for pen_dir in claude_pens(ctx, &entry.slug)? {
                held.extend(ctx.backend.confined(&pen_dir).read()?.map(|file| file.oauth));
            }
        }
        Provider::Codex => {
            for store in codex_copies(ctx, entry)? {
                held.extend(store.read()?.and_then(|file| file.oauth()));
            }
        }
    }
    Ok(held)
}

fn claude_pens(ctx: &Ctx, slug: &str) -> Result<Vec<PathBuf>> {
    let root = ctx.stash.root().join("pens");
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut pens = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        if pen::account_of(&path).as_deref() == Some(slug) {
            pens.push(path);
        }
    }
    Ok(pens)
}

/// Resolve a pen from its credentials, never its directory or mutable marker.
/// A token already in the stash needs no network call; a newly rotated token
/// needs the profile endpoint to prove which account owns it.
fn claude_owner(
    accounts: &[Stashed],
    oauth: &Oauth,
    marked: &str,
    profile_uuid: &impl Fn(&Oauth) -> Result<String>,
) -> Result<String> {
    let matches: Vec<_> = accounts
        .iter()
        .filter(|entry| {
            entry.account.provider == Provider::Claude
                && entry.account.oauth.refresh_token == oauth.refresh_token
                && entry.account.oauth.access_token == oauth.access_token
        })
        .collect();
    if let [entry] = matches.as_slice()
        && entry.slug == marked
    {
        return Ok(marked.to_string());
    }
    let uuid = profile_uuid(oauth)?;
    claude_owner_uuid(accounts, &uuid)
}

fn claude_owner_uuid(accounts: &[Stashed], uuid: &str) -> Result<String> {
    let matches: Vec<_> = accounts
        .iter()
        .filter(|entry| entry.account.provider == Provider::Claude && entry.account.uuid == uuid)
        .collect();
    match matches.as_slice() {
        [entry] => Ok(entry.slug.clone()),
        [] => bail!("Claude pen belongs to an unstashed account UUID {uuid}"),
        _ => bail!("multiple Claude stash slots name account UUID {uuid}"),
    }
}

fn verify_claude_pens(
    ctx: &Ctx,
    accounts: &[Stashed],
    profile_uuid: &impl Fn(&Oauth) -> Result<String>,
) -> Result<()> {
    let root = ctx.stash.root().join("pens");
    if !root.is_dir() {
        return Ok(());
    }
    let mut corrections = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        let Some(marked) = pen::account_of(&path) else { continue };
        let Some(file) = ctx.backend.confined(&path).read()? else { continue };
        let owner = claude_owner(accounts, &file.oauth, &marked, profile_uuid)
            .with_context(|| format!("verifying Claude pen {}", path.display()))?;
        if marked != owner {
            corrections.push((path, owner));
        }
    }
    // Validate the entire set before changing any marker.
    for (path, owner) in corrections {
        pen::set_account(&path, &owner)?;
    }
    Ok(())
}

/// Recover a stash slot from the newest pen whose credentials prove the same
/// Claude UUID. The duplicate-token check is performed on the proposed whole
/// stash before touching any account file.
pub fn repair(ctx: &Ctx) -> Result<()> {
    let repaired =
        repair_with(ctx, &|oauth| Ok(ctx.api.profile(&oauth.access_token)?.account.uuid))?;
    if repaired.is_empty() {
        println!("Claude stash already agrees with its verified pens");
    } else {
        println!("restored {} from UUID-verified pens", repaired.join(", "));
    }
    Ok(())
}

fn repair_with(ctx: &Ctx, profile_uuid: &impl Fn(&Oauth) -> Result<String>) -> Result<Vec<String>> {
    let mut accounts = ctx.stash.list()?;
    let root = ctx.stash.root().join("pens");
    let mut candidates: std::collections::HashMap<String, Oauth> = std::collections::HashMap::new();
    let mut markers = Vec::new();
    if root.is_dir() {
        for entry in std::fs::read_dir(root)? {
            let path = entry?.path();
            let Some(marked) = pen::account_of(&path) else { continue };
            let Some(file) = ctx.backend.confined(&path).read()? else { continue };
            // Repair is deliberately stricter than routine reconciliation:
            // the stash itself may be corrupt, so matching its token is not
            // evidence of a pen's owner.
            let uuid = profile_uuid(&file.oauth)
                .with_context(|| format!("verifying Claude pen {}", path.display()))?;
            let owner = claude_owner_uuid(&accounts, &uuid)?;
            if marked != owner {
                markers.push((path, owner.clone()));
            }
            let candidate = candidates.entry(owner).or_insert_with(|| file.oauth.clone());
            if file.oauth.expires_at > candidate.expires_at {
                *candidate = file.oauth;
            }
        }
    }
    let mut changed = Vec::new();
    for i in 0..accounts.len() {
        if accounts[i].account.provider != Provider::Claude {
            continue;
        }
        let duplicate = accounts.iter().enumerate().any(|(j, other)| {
            i != j
                && other.account.provider == Provider::Claude
                && accounts[i].account.uuid != other.account.uuid
                && accounts[i].account.oauth.refresh_token == other.account.oauth.refresh_token
        });
        let Some(candidate) = candidates.get(&accounts[i].slug) else { continue };
        if (duplicate || candidate.expires_at > accounts[i].account.oauth.expires_at)
            && candidate.refresh_token != accounts[i].account.oauth.refresh_token
        {
            accounts[i].account.oauth = candidate.clone();
            changed.push(i);
        }
    }
    validate_claude_credentials(&accounts)?;
    let backup = ctx.stash.root().join("repair-backups");
    if !changed.is_empty() {
        std::fs::create_dir_all(&backup)?;
        std::fs::set_permissions(&backup, std::fs::Permissions::from_mode(0o700))?;
    }
    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos();
    let mut repaired = Vec::new();
    for i in changed {
        let entry = &accounts[i];
        let old = ctx.stash.root().join("accounts").join(format!("{}.json", entry.slug));
        let saved = backup.join(format!("{}-{nonce}.json", entry.slug));
        std::fs::copy(&old, &saved)?;
        std::fs::set_permissions(&saved, std::fs::Permissions::from_mode(0o600))?;
        ctx.stash.save(&entry.slug, &entry.account)?;
        repaired.push(entry.slug.clone());
    }
    for (path, owner) in markers {
        pen::set_account(&path, &owner)?;
    }
    Ok(repaired)
}

/// Pins may have been switched in place, so their credentials' account id,
/// rather than the directory name, decides which account owns the copy.
fn codex_copies(ctx: &Ctx, entry: &Stashed) -> Result<Vec<codex::Store>> {
    let root = ctx.stash.root().join("pens");
    let mut directories = Vec::new();
    if let Some(home) = pen::codex_home_of(ctx.codex.dir())? {
        directories.push(home);
    }
    if root.is_dir() {
        for directory in std::fs::read_dir(root)? {
            let directory = directory?.path();
            if pen::codex_home_of(&directory)?.is_some() {
                directories.push(directory);
            }
        }
    }
    let mut stores = Vec::new();
    for directory in directories {
        let store = codex::Store::at(&directory, Some(&directory));
        let held = store.read()?.and_then(|file| file.oauth());
        let belongs_to_account =
            held.as_ref().is_some_and(|oauth| codex_account_matches(entry, oauth));
        if belongs_to_account {
            stores.push(store);
        }
    }
    Ok(stores)
}

/// A workspace can contain several users. Its account id alone cannot
/// identify which user's credentials a stashed login owns.
fn codex_account_matches(entry: &Stashed, oauth: &Oauth) -> bool {
    let same_workspace = oauth.account_id() == Some(entry.account.uuid.as_str());
    let identity = oauth.id_token().and_then(|token| codex::identity(token).ok());
    let same_user =
        identity.is_some_and(|who| who.email.eq_ignore_ascii_case(&entry.account.email));
    same_workspace && same_user
}

/// The most recently minted of some copies of one account's credentials.
///
/// A refresh mints an access token that outlives the one it replaces, so the
/// furthest expiry marks the copy carrying the refresh token the server still
/// honours; the rest were spent producing it.
fn newest(copies: impl IntoIterator<Item = Oauth>) -> Option<Oauth> {
    copies.into_iter().max_by_key(|oauth| oauth.expires_at)
}

/// Hand `oauth` to every copy of this account's credentials: the stash entry,
/// `entry` itself, the live credentials when it is the installed account, and
/// its pen when it has one.
///
/// Updating `entry` in place is what stops a later step installing the copy
/// that was read beforehand. A copy left on a superseded token buys nothing,
/// and a session holding one is a session that has to log in again.
fn propagate(ctx: &Ctx, entry: &mut Stashed, oauth: &Oauth, live: &Live) -> Result<()> {
    entry.account.oauth = oauth.clone();
    ctx.stash.save(&entry.slug, &entry.account)?;

    if live.holds(entry) {
        install_live(ctx, entry.account.provider, oauth)?;
    }
    match entry.account.provider {
        Provider::Claude => {
            for pen in claude_pens(ctx, &entry.slug)? {
                install(ctx.backend.confined(&pen).as_ref(), oauth)?;
            }
        }
        Provider::Codex => {
            for store in codex_copies(ctx, entry)? {
                install_codex(&store, oauth)?;
            }
        }
    }
    Ok(())
}

/// Install `oauth` into its provider's live slot.
fn install_live(ctx: &Ctx, provider: Provider, oauth: &Oauth) -> Result<()> {
    match provider {
        Provider::Claude => install(ctx.creds, oauth),
        Provider::Codex => install_codex(ctx.codex, oauth),
    }
}

/// Install `oauth` into Codex's login, keeping every other field the file
/// has, under a lock of this tool's own in the same directory.
fn install_codex(store: &dyn codex::Creds, oauth: &Oauth) -> Result<()> {
    std::fs::create_dir_all(store.dir())
        .with_context(|| format!("creating {}", store.dir().display()))?;
    let _guard = lock::acquire(store.dir())?;
    let current = store.read()?.unwrap_or_default();
    store.write(&current.with(oauth))
}

/// Bring every copy of every account onto the newest credentials on disk for
/// it, before any of them is presented to the server.
///
/// This is the standing repair for the copies this tool does not write: a
/// session refreshing its own credentials leaves every other copy of that
/// account behind, and nothing says so until one of them is used.
fn reconcile(ctx: &Ctx, accounts: &mut [Stashed], live: &Live) -> Result<()> {
    reconcile_with(ctx, accounts, live, &|oauth| {
        Ok(ctx.api.profile(&oauth.access_token)?.account.uuid)
    })
}

fn reconcile_with(
    ctx: &Ctx,
    accounts: &mut [Stashed],
    live: &Live,
    profile_uuid: &impl Fn(&Oauth) -> Result<String>,
) -> Result<()> {
    validate_claude_credentials(accounts)?;
    verify_claude_pens(ctx, accounts, profile_uuid)?;
    let installed = slots(ctx)?;
    let claude_tokens: Vec<_> = accounts
        .iter()
        .filter(|entry| entry.account.provider == Provider::Claude)
        .map(|entry| {
            (
                entry.account.oauth.refresh_token.clone(),
                entry.account.uuid.clone(),
                entry.slug.clone(),
            )
        })
        .collect();
    for entry in accounts.iter_mut() {
        let mine = live.holds(entry).then(|| installed.of(entry.account.provider)).flatten();
        let held = copies(ctx, entry, mine)?;
        let Some(newest) = newest(held.iter().cloned()) else { continue };
        if entry.account.provider == Provider::Claude
            && let Some((_, _, other)) = claude_tokens.iter().find(|(token, uuid, _)| {
                token == &newest.refresh_token && uuid != &entry.account.uuid
            })
        {
            bail!("{}'s pen holds {}'s credentials; refusing to copy them", entry.slug, other);
        }
        if held.iter().all(|oauth| oauth.refresh_token == newest.refresh_token) {
            continue;
        }
        propagate(ctx, entry, &newest, live)?;
    }
    Ok(())
}

/// Write back every token a probe refreshed.
fn persist(ctx: &Ctx, accounts: &mut [Stashed], probes: &[Probe], live: &Live) -> Result<()> {
    for probe in probes {
        let Some(oauth) = &probe.refreshed else { continue };
        let Some(entry) = accounts.iter_mut().find(|a| a.slug == probe.slug) else { continue };
        propagate(ctx, entry, oauth, live)?;
    }
    Ok(())
}

/// Which stashed account of `provider` its live slot holds.
///
/// A token match is direct evidence and wins; the recorded pointer is the
/// fallback for the moment right after a refresh rotated one copy.
fn identify(
    ctx: &Ctx,
    accounts: &[Stashed],
    provider: Provider,
    live: Option<&Oauth>,
) -> Option<String> {
    let live = live?;
    if provider == Provider::Codex {
        return accounts
            .iter()
            .find(|a| a.account.provider == provider && codex_account_matches(a, live))
            .map(|a| a.slug.clone());
    }
    let matched = accounts.iter().filter(|a| a.account.provider == provider).find(|a| {
        a.account.oauth.refresh_token == live.refresh_token
            || a.account.oauth.access_token == live.access_token
    });
    if let Some(entry) = matched {
        return Some(entry.slug.clone());
    }
    let recorded = ctx.stash.active(provider)?;
    accounts
        .iter()
        .any(|a| a.slug == recorded && a.account.provider == provider)
        .then_some(recorded)
}

/// Put `oauth` into a credentials file, keeping every unrelated field the
/// existing one carries.
fn merged(current: Option<CredsFile>, oauth: &Oauth) -> CredsFile {
    match current {
        Some(file) => CredsFile { oauth: oauth.clone(), ..file },
        None => CredsFile::new(oauth.clone()),
    }
}

/// Install `oauth` into a credential store, under the lock that guards the
/// configuration directory it answers for.
fn install(store: &dyn CredStore, oauth: &Oauth) -> Result<()> {
    let _guard = lock::acquire(store.dir())?;
    let current = store.read()?;
    store.write(&merged(current, oauth))
}

/// The credentials in use, together with their freshest access token.
///
/// The file comes back as it was read rather than as it now stands, because
/// working out which stashed account it is has to match against the tokens the
/// stash still holds. A refresh here rotates the token running sessions are
/// holding, so it is mirrored back to them before anything else happens.
fn in_use(ctx: &Ctx, hint: &str) -> Result<(CredsFile, Oauth)> {
    let Some(file) = ctx.creds.read()? else {
        bail!("no credentials in {}; {hint}", ctx.creds.describe());
    };
    let Some(oauth) = freshen(ctx.apis(), Provider::Claude, &file.oauth)? else {
        let oauth = file.oauth.clone();
        return Ok((file, oauth));
    };
    install(ctx.creds, &oauth)?;
    Ok((file, oauth))
}

/// Codex's login in use, with its freshest access token; a refresh here is
/// mirrored back to the file Codex reads.
fn codex_in_use(ctx: &Ctx, hint: &str) -> Result<Oauth> {
    let Some(file) = ctx.codex.read()? else {
        bail!("no Codex login in {}; {hint}", ctx.codex.describe());
    };
    let Some(oauth) = file.oauth() else {
        bail!("the login in {} is an API key, not a ChatGPT account; {hint}", ctx.codex.describe());
    };
    match freshen(ctx.apis(), Provider::Codex, &oauth)? {
        None => Ok(oauth),
        Some(fresh) => {
            install_codex(ctx.codex, &fresh)?;
            Ok(fresh)
        }
    }
}

// ── commands ────────────────────────────────────────────────────────────────

pub fn list(ctx: &Ctx, json: bool, cached: bool) -> Result<()> {
    if cached {
        let readings = readings(ctx)?;
        if json {
            let views: Vec<View> =
                readings.iter().map(|r| view(&r.entry, r.polled_at.as_deref())).collect();
            println!("{}", serde_json::to_string_pretty(&views)?);
            return Ok(());
        }
        let table = Table::build(readings.into_iter().map(|r| r.entry).collect(), Style::detect());
        println!("{}", table.lines(None).join("\n"));
        return Ok(());
    }
    let mut accounts = stashed(ctx)?;
    let (table, _) = survey(ctx, &mut accounts, Style::detect())?;
    if json {
        return emit_json(&table);
    }
    println!("{}", table.lines(None).join("\n"));
    Ok(())
}

/// An entry as the last poll left it, and when that was.
pub struct Cached {
    pub entry: Entry,
    pub polled_at: Option<String>,
}

/// Poll usage using the normal provider APIs, without rotating accounts or sending notices.
pub fn refresh_readings(ctx: &Ctx) -> Result<Vec<Cached>> {
    let mut accounts = stashed(ctx)?;
    let (table, _) = survey(ctx, &mut accounts, Style::detect())?;
    table
        .entries()
        .iter()
        .map(|entry| {
            let polled_at = if entry.usage.is_ok() {
                ctx.usage.read(&entry.slug)?.map(|reading| reading.polled_at)
            } else {
                None
            };
            Ok(Cached { entry: entry.clone(), polled_at })
        })
        .collect()
}

/// Provider-native catalog shared by the GUI and terminal route editors.
pub fn model_ids(ctx: &Ctx, provider: Provider) -> Result<Vec<String>> {
    let active = ctx.stash.active(provider);
    let account = ctx
        .stash
        .list()?
        .into_iter()
        .filter(|a| a.account.provider == provider)
        .max_by_key(|a| active.as_deref() == Some(a.slug.as_str()));
    let Some(account) = account else {
        return Ok(vec![]);
    };
    let oauth = &account.account.oauth;
    match provider {
        Provider::Claude => ctx.api.models(&oauth.access_token),
        Provider::Codex => {
            let cache = std::fs::read(ctx.codex.dir().join("models_cache.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
            let version = cache
                .as_ref()
                .and_then(|c| c["client_version"].as_str())
                .filter(|v| !v.trim().is_empty())
                .context("Open Codex once to initialize its model catalog, then Refresh")?;
            let account_id = oauth.account_id().context("Codex account ID is missing")?;
            ctx.codex_api.models(&oauth.access_token, account_id, version)
        }
    }
}

/// Upsert one model route, preserving every other route and both active logins.
pub fn save_route(ctx: &Ctx, rule: crate::routing::Rule) -> Result<()> {
    let accounts = ctx.stash.list()?;
    for slug in &rule.accounts {
        if !accounts.iter().any(|a| a.slug == *slug && a.account.provider == rule.provider) {
            bail!("No {} account named {slug}", rule.provider);
        }
    }
    let mut routes = crate::routing::Routing::read(ctx.stash.root())?;
    if let Some(existing) =
        routes.rules.iter_mut().find(|r| r.provider == rule.provider && r.model == rule.model)
    {
        *existing = rule;
    } else {
        routes.rules.push(rule);
    }
    routes.write(ctx.stash.root())
}

/// Every account with what the last poll wrote down for it: no network, no
/// refresh, nothing spent. The source of truth for anything that asks more
/// often than a poll can be afforded, with `ccs watch` keeping it current.
/// The account in use is whichever the stash's pointer names; identifying it
/// against the live credentials would cost a keychain read per call.
pub fn readings(ctx: &Ctx) -> Result<Vec<Cached>> {
    let accounts = stashed(ctx)?;
    accounts
        .iter()
        .map(|account| {
            let live = ctx.stash.active(account.account.provider);
            let reading = ctx.usage.read(&account.slug)?;
            let (usage, polled_at) = match reading {
                Some(reading) => (Ok(reading.usage), Some(reading.polled_at)),
                None => (Err("not polled yet; `ccs watch` polls".to_string()), None),
            };
            let entry = Entry {
                provider: account.account.provider,
                slug: account.slug.clone(),
                email: account.account.email.clone(),
                plan: account.account.plan_label(),
                active: live.as_deref() == Some(account.slug.as_str()),
                usage,
            };
            Ok(Cached { entry, polled_at })
        })
        .collect()
}

/// The limits of every account in use: the one in Claude Code's slot, the
/// one in Codex's, whichever are logged in.
pub fn status(ctx: &Ctx, json: bool, cached: bool, provider: Option<Provider>) -> Result<()> {
    let readings = status_entries(ctx, cached, provider)?;
    if readings.is_empty() {
        bail!(
            "no matching account is logged in; use `ccs add --current` or `ccs add --current --codex`"
        );
    }
    if json {
        let views: Vec<_> =
            readings.iter().map(|r| view(&r.entry, r.polled_at.as_deref())).collect();
        let mut top = serde_json::to_value(&views[0])?;
        if let Some(codex) = views.get(1) {
            top["codex"] = serde_json::to_value(codex)?;
        }
        println!("{}", serde_json::to_string_pretty(&top)?);
        return Ok(());
    }
    let style = Style::detect();
    for (index, reading) in readings.iter().enumerate() {
        if index > 0 {
            println!();
        }
        let entry = &reading.entry;
        println!(
            "{} · {}  {}",
            entry.provider.label(),
            style.bold(&entry.email),
            style.dim(&entry.plan)
        );
        for line in render::detail(entry, style) {
            println!("{line}");
        }
        if let Some(polled_at) = &reading.polled_at {
            println!("last polled {polled_at}");
        }
    }
    Ok(())
}

/// Cached status reads the calling client's credentials to select the account,
/// but never refreshes or polls. A pin can differ from the global active pointer.
fn status_entries(ctx: &Ctx, cached: bool, selected: Option<Provider>) -> Result<Vec<Cached>> {
    let accounts = ctx.stash.list()?;
    validate_claude_credentials(&accounts)?;
    let mut readings = Vec::new();
    for provider in Provider::ALL {
        if selected.is_some_and(|selected| provider != selected) {
            continue;
        }
        let Some(held) = slot(ctx, provider)? else { continue };
        let identified = identify(ctx, &accounts, provider, Some(&held));
        let account =
            identified.as_ref().and_then(|slug| accounts.iter().find(|a| &a.slug == slug));
        if cached {
            let account = account.context("the current login is not stashed; run `ccs add --current` (with --codex for Codex)")?;
            let reading = ctx.usage.read(&account.slug)?;
            let (usage, polled_at) = match reading {
                Some(reading) => (Ok(reading.usage), Some(reading.polled_at)),
                None => (Err("not polled yet; `ccs watch` polls".to_string()), None),
            };
            readings.push(Cached {
                entry: Entry {
                    provider,
                    slug: account.slug.clone(),
                    email: account.account.email.clone(),
                    plan: account.account.plan_label(),
                    active: true,
                    usage,
                },
                polled_at,
            });
            continue;
        }
        if let Some(account) = account {
            let mut current = account.clone();
            current.account.oauth = newest(copies(ctx, account, Some(&held))?).expect("live copy");
            let result = probe(ctx, &current);
            if let Some(oauth) = &result.refreshed {
                let mut live = Live::default();
                live.set(provider, Some(current.slug.clone()));
                propagate(ctx, &mut current, oauth, &live)?;
            }
            let polled_at = ctx.usage.read(&current.slug)?.map(|r| r.polled_at);
            readings.push(Cached { entry: entry_of(&current, &result, true), polled_at });
            continue;
        }
        let copies = match account {
            Some(account) => copies(ctx, account, Some(&held))?,
            None => vec![held.clone()],
        };
        let latest = newest(copies.iter().cloned()).expect("includes the live credentials");
        let oauth = freshen(ctx.apis(), provider, &latest)?.unwrap_or(latest);
        let credentials_changed = copies.iter().any(|copy| {
            copy.access_token != oauth.access_token || copy.refresh_token != oauth.refresh_token
        });
        if credentials_changed {
            match account {
                Some(account) => {
                    let mut live = Live::default();
                    live.set(provider, Some(account.slug.clone()));
                    propagate(ctx, &mut account.clone(), &oauth, &live)?;
                }
                None => install_live(ctx, provider, &oauth)?,
            }
        }
        let (email, plan) = match provider {
            Provider::Claude => {
                warn_overridden();
                let profile = ctx.api.profile(&oauth.access_token)?;
                let organization = profile.organization.as_ref();
                (
                    profile.account.email,
                    plan_label(
                        organization.and_then(|o| o.rate_limit_tier.as_deref()),
                        organization.and_then(|o| o.organization_type.as_deref()),
                    ),
                )
            }
            Provider::Codex => {
                let who = codex::identity(
                    oauth.id_token().context("Codex login has no identity token")?,
                )?;
                (who.email, format!("codex {}", who.plan))
            }
        };
        let usage = read_usage(ctx.apis(), provider, &oauth)?;
        if let Some(slug) = &identified {
            ctx.usage.record(slug, &usage)?;
        }
        readings.push(Cached {
            entry: Entry {
                provider,
                slug: identified.unwrap_or_default(),
                email,
                plan,
                active: true,
                usage: Ok(usage),
            },
            polled_at: None,
        });
    }
    Ok(readings)
}

/// Stash an account. Two ways in: log in to another one without disturbing the
/// account in use, or capture whichever account is in use right now.
pub fn add(
    ctx: &Ctx,
    provider: Provider,
    name: Option<&str>,
    current: bool,
    options: &login::Options,
) -> Result<()> {
    options.validate(provider)?;
    let oauth = match (provider, current) {
        (Provider::Claude, true) => {
            in_use(ctx, "sign in with `claude` before `ccs add --current`")?.1
        }
        (Provider::Claude, false) => {
            println!("logging in to another Claude Code account; the one in use is not affected");
            login::run(ctx.backend, ctx.stash.root(), &claude_binary(), options)?
        }
        (Provider::Codex, true) => {
            codex_in_use(ctx, "sign in with `codex login` before `ccs add --current --codex`")?
        }
        (Provider::Codex, false) => {
            println!("logging in to another Codex account; the one in use is not affected");
            login::run_codex(ctx.backend, ctx.stash.root(), &codex_binary())?
        }
    };
    let recorded = record(ctx, provider, oauth, name, Installed::from(current))?;
    let verb = if recorded.replaced { "refreshed" } else { "stashed" };
    let (email, slug) = (&recorded.stashed.account.email, &recorded.stashed.slug);

    match current {
        true => println!("{verb} the {provider} account in use, {email}, as {slug}"),
        false => println!(
            "{verb} {provider} account {email} as {slug}; `ccs use {slug}` to switch to it"
        ),
    }
    Ok(())
}

/// Whether the credentials being stashed are the ones currently installed.
///
/// A login mints an account that is stashed but *not* in use. Recording it as
/// the installed one would make the next switch fold the live tokens into the
/// wrong entry, so the two paths have to stay distinguishable.
#[derive(PartialEq)]
enum Installed {
    Yes,
    No,
}

impl From<bool> for Installed {
    fn from(current: bool) -> Self {
        match current {
            true => Self::Yes,
            false => Self::No,
        }
    }
}

#[derive(Debug)]
struct Recorded {
    stashed: Stashed,
    /// The slug already held an account, so this refreshed it in place rather
    /// than adding one.
    replaced: bool,
}

/// Which Claude Code to drive, overridable for a non-standard install. Held by
/// the orchestrator because both the login and the pinned session are launched
/// from here.
fn claude_binary() -> String {
    std::env::var("CCS_CLAUDE_BINARY").unwrap_or_else(|_| "claude".to_string())
}

fn codex_binary() -> String {
    std::env::var("CCS_CODEX_BINARY").unwrap_or_else(|_| "codex".to_string())
}

/// Ask who some credentials belong to, then write them into the stash.
///
/// Claude is asked over the network; a Codex account names itself in its
/// identity token. A slug already used by the other provider — the same
/// email signed up twice — gets the provider's name in front.
fn record(
    ctx: &Ctx,
    provider: Provider,
    oauth: Oauth,
    name: Option<&str>,
    installed: Installed,
) -> Result<Recorded> {
    let (email, uuid, plan, rate_limit_tier) = match provider {
        Provider::Claude => {
            let profile = ctx.api.profile(&oauth.access_token)?;
            let organization = profile.organization.as_ref();
            (
                profile.account.email.clone(),
                profile.account.uuid.clone(),
                organization.and_then(|o| o.organization_type.clone()),
                organization.and_then(|o| o.rate_limit_tier.clone()),
            )
        }
        Provider::Codex => {
            let token = oauth.id_token().context("a Codex login without an identity token")?;
            let who = codex::identity(token)?;
            (who.email, who.account_id, Some(who.plan), None)
        }
    };
    let held = ctx.stash.list()?;
    let slug = match name {
        Some(name) => {
            if let Some(other) =
                held.iter().find(|s| s.slug == name && s.account.provider != provider)
            {
                bail!(
                    "{name} is a {} account, {}; pick another name",
                    other.account.provider,
                    other.account.email
                );
            }
            name.to_string()
        }
        None => {
            let plain = stash::slugify(&email);
            let taken_by_other =
                held.iter().any(|s| s.slug == plain && s.account.provider != provider);
            match taken_by_other {
                true => format!("{provider}-{plain}"),
                false => plain,
            }
        }
    };
    let replaced = held.iter().any(|s| s.slug == slug);

    let account = Account {
        provider,
        email,
        uuid,
        plan,
        rate_limit_tier,
        added_at: Timestamp::now().to_string(),
        oauth,
    };
    ctx.stash.save(&slug, &account)?;
    let is_codex_pin =
        provider == Provider::Codex && pen::codex_home_of(ctx.codex.dir())?.is_some();
    let installed_globally = installed == Installed::Yes && !is_codex_pin;
    if installed_globally {
        ctx.stash.set_active(provider, &slug)?;
    }
    Ok(Recorded { stashed: Stashed { slug, account }, replaced })
}

pub fn remove(ctx: &Ctx, needle: &str) -> Result<()> {
    let accounts = ctx.stash.list()?;
    let target = stash::resolve(&accounts, needle)?.clone();
    ctx.stash.remove(&target.slug)?;
    if let Err(error) = forget_pins(ctx, &target) {
        eprintln!(
            "ccs: {}'s pinned credentials are still there: {}",
            target.slug,
            describe(&error)
        );
    }
    if let Err(e) = ctx.usage.forget(&target.slug) {
        eprintln!("ccs: {}'s last usage reading is still there: {}", target.slug, describe(&e));
    }
    println!("forgot {} ({})", target.account.email, target.slug);
    Ok(())
}

fn forget_pins(ctx: &Ctx, target: &Stashed) -> Result<()> {
    match target.account.provider {
        Provider::Claude => {
            pen::discard(ctx.stash.root(), &target.slug)?;
            ctx.backend.forget(&pen::at(ctx.stash.root(), &target.slug))
        }
        Provider::Codex => {
            for store in codex_copies(ctx, target)? {
                if pen::codex_home_of(store.dir())?.is_none() {
                    continue;
                }
                std::fs::remove_file(store.dir().join("auth.json"))?;
            }
            Ok(())
        }
    }
}

pub fn use_account(ctx: &Ctx, needle: &str, force: bool) -> Result<()> {
    let switched = switch(ctx, needle, force)?;
    report_switch(&switched.target, switched.told);
    Ok(())
}

/// What a switch came to.
pub struct Switched {
    pub target: Stashed,
    /// Sessions that heard about it.
    pub told: usize,
}

/// Switch to the account `needle` names, probing it first so a spent one is
/// refused — or, off a terminal, refused without asking — unless `force`.
pub fn switch(ctx: &Ctx, needle: &str, force: bool) -> Result<Switched> {
    let mut accounts = ctx.stash.list()?;
    let slug = stash::resolve(&accounts, needle)?.slug.clone();

    let live = identify_live(ctx, &accounts)?;
    reconcile(ctx, &mut accounts, &live)?;
    let probe = probe(ctx, stash::resolve(&accounts, &slug)?);
    persist(ctx, &mut accounts, std::slice::from_ref(&probe), &live)?;

    // Read after the probe has been folded back in, never before it: the probe
    // may have refreshed this very account, and the copy taken beforehand
    // carries the tokens that refresh superseded.
    let target = stash::resolve(&accounts, &slug)?.clone();
    guard_exhausted(&entry_of(&target, &probe, false), force)?;
    let told = switch_to(ctx, &mut accounts, &target)?;
    Ok(Switched { target, told })
}

/// Subscribe (or unsubscribe) the calling client session to notices.
pub fn notify(
    ctx: &Ctx,
    off: bool,
    kinds: &[String],
    bypass: bool,
    provider: Option<Provider>,
) -> Result<()> {
    let session = notify::Session::detect(
        ctx.creds.dir(),
        ctx.codex.dir(),
        &codex_binary(),
        provider,
        bypass,
    )?;
    notify::subscribe(ctx.stash.root(), session, !off, kinds)
}

/// The slugs `names` resolve to, for a pool.
pub fn resolve_pool(ctx: &Ctx, names: &[String]) -> Result<Vec<String>> {
    let accounts = stashed(ctx)?;
    names.iter().map(|n| stash::resolve(&accounts, n).map(|s| s.slug.clone())).collect()
}

/// What one turn of the watch loop found and did.
pub struct Poll {
    /// Every account as the poll read it; empty when the poll itself failed.
    pub entries: Vec<Entry>,
    /// The notices raised, delivered to the sessions that asked, with how
    /// many heard each.
    pub notices: Vec<(watch::Event, usize)>,
    /// The accounts rotated onto, or why a rotation failed.
    pub rotations: Vec<Result<Switched, String>>,
    /// Why the poll failed, when it did.
    pub failed: Option<String>,
}

/// One turn of the watch: poll every account, raise a notice for whatever
/// changed since `before`, and rotate any pooled account that has run high.
/// `before` is left holding this turn for the next.
pub fn poll(ctx: &Ctx, before: &mut watch::Snapshot, high: f64, pool: &[String]) -> Poll {
    let mut turn =
        Poll { entries: Vec::new(), notices: Vec::new(), rotations: Vec::new(), failed: None };
    let mut accounts = match stashed(ctx) {
        Ok(accounts) => accounts,
        Err(e) => {
            turn.failed = Some(describe(&e));
            return turn;
        }
    };
    let table = match survey(ctx, &mut accounts, Style::detect()) {
        Ok((table, _)) => table,
        Err(e) => {
            turn.failed = Some(describe(&e));
            return turn;
        }
    };
    for event in watch::diff(before, table.entries(), high) {
        let told = notify::broadcast(
            match event.provider {
                Provider::Claude => ctx.creds.dir(),
                Provider::Codex => ctx.codex.dir(),
            },
            ctx.stash.root(),
            event.provider,
            event.kind,
            &event.text,
        );
        turn.notices.push((event, told));
    }
    *before = watch::snapshot(table.entries());
    for next in watch::rotations(table.entries(), pool, high) {
        let rotated = stash::resolve(&accounts, &next.slug).cloned().and_then(|target| {
            let told = switch_to(ctx, &mut accounts, &target)?;
            Ok(Switched { target, told })
        });
        turn.rotations.push(rotated.map_err(|e| describe(&e)));
    }
    turn.entries = table.entries().to_vec();
    turn
}

/// Poll every account on an interval and raise a notice for whatever changed.
/// Runs until killed; each notice is echoed here as well as delivered.
pub fn watch(ctx: &Ctx, every: Duration, high: f64, rotate: &[String]) -> Result<()> {
    let pool = resolve_pool(ctx, rotate)?;
    if !pool.is_empty() {
        println!("{} rotating between {}", stamp(), pool.join(", "));
    }
    let mut before = watch::Snapshot::new();
    loop {
        let turn = poll(ctx, &mut before, high, &pool);
        if let Some(why) = &turn.failed {
            eprintln!("{} poll failed: {why}", stamp());
        }
        for (event, told) in &turn.notices {
            println!("{} {}: {}{}", stamp(), event.kind, event.text, heard(*told));
        }
        for rotated in &turn.rotations {
            match rotated {
                Ok(switched) => println!(
                    "{} rotate: switched to {} ({}){}",
                    stamp(),
                    switched.target.account.email,
                    switched.target.slug,
                    heard(switched.told)
                ),
                Err(why) => eprintln!("{} rotate failed: {why}", stamp()),
            }
        }
        thread::sleep(every);
    }
}

fn stamp() -> String {
    Timestamp::now().strftime("%H:%M:%S").to_string()
}

// ── the gateway ─────────────────────────────────────────────────────────────

/// Serve the API on loopback as the account in use, until killed.
///
/// The connections are answered on threads of their own; this thread keeps
/// the stash, and answers their questions about which account to send as.
pub fn serve(ctx: &Ctx, port: u16, rotate: &[String]) -> Result<()> {
    let pool = resolve_pool(ctx, rotate)?;
    let keys = gateway_keys(ctx)?;
    let listener = std::net::TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("listening on 127.0.0.1:{port}"))?;

    println!("{} serving the API on http://127.0.0.1:{port} as the account in use", stamp());
    if !pool.is_empty() {
        println!("{} falling over to {} when it is limited", stamp(), pool.join(", "));
    }
    println!("paste into pi's models.json:\n{}", serve::pi_config(port));

    let (asks, inbox) = std::sync::mpsc::channel();
    serve::listen(listener, keys, asks);
    desk(ctx, &pool, inbox);
    Ok(())
}

/// Both gateway keys, minted if need be.
pub fn gateway_keys(ctx: &Ctx) -> Result<serve::Keys> {
    Ok(serve::Keys {
        claude: serve::key(ctx.stash.root())?,
        codex: serve::codex_key(ctx.stash.root())?,
    })
}

/// Answer the gateway's questions about which account to send as, until the
/// listener that asks them is gone. The thread that holds the stash runs
/// this; the connection threads ask.
pub fn desk(ctx: &Ctx, pool: &[String], inbox: std::sync::mpsc::Receiver<serve::Ask>) {
    let desk = Desk { ctx, pool };
    for ask in inbox {
        ask.answer(&desk);
    }
}

/// Print a provider's gateway key, for pi's `"apiKey": "!ccs serve --key"`.
pub fn serve_key(ctx: &Ctx, provider: Provider) -> Result<()> {
    let key = match provider {
        Provider::Claude => serve::key(ctx.stash.root())?,
        Provider::Codex => serve::codex_key(ctx.stash.root())?,
    };
    println!("{key}");
    Ok(())
}

/// The stash as the gateway sees it: which account a request goes out as.
struct Desk<'a> {
    ctx: &'a Ctx<'a>,
    /// Accounts to fall over to, in order, when the one in use is limited.
    pool: &'a [String],
}

impl Desk<'_> {
    /// Which stashed account is in use. The recorded pointer is what every
    /// switch writes, and is cheap; the live credentials are read only when
    /// it points nowhere.
    fn live(&self, accounts: &[Stashed]) -> Result<Live> {
        let mut live = Live::default();
        for provider in Provider::ALL {
            let recorded = self.ctx.stash.active(provider).filter(|s| {
                accounts.iter().any(|a| a.slug == *s && a.account.provider == provider)
            });
            let slug = match recorded {
                Some(slug) => Some(slug),
                None => identify(self.ctx, accounts, provider, slot(self.ctx, provider)?.as_ref()),
            };
            live.set(provider, slug);
        }
        Ok(live)
    }

    /// The account of `provider` a request goes out as: the one in use,
    /// unless it is among `avoid`, and then the first pooled account of that
    /// provider that is not.
    fn choose(
        &self,
        accounts: &[Stashed],
        provider: Provider,
        live: Option<&str>,
        avoid: &[String],
    ) -> Result<String> {
        let usable = |slug: &str| {
            accounts.iter().any(|a| a.slug == slug && a.account.provider == provider)
                && !avoid.iter().any(|a| a == slug)
        };
        if let Some(slug) = live.filter(|s| usable(s)) {
            return Ok(slug.to_string());
        }
        if let Some(slug) = self.pool.iter().find(|s| usable(s)) {
            return Ok(slug.clone());
        }
        match (live, avoid.is_empty()) {
            (None, true) => bail!("no {provider} account is in use; `ccs use` one"),
            _ => bail!("every account is limited: {}", avoid.join(", ")),
        }
    }

    /// `slug`'s credentials, refreshed if they are spent.
    ///
    /// A session may have refreshed them already, leaving the stash behind on
    /// a spent refresh token; the copies are brought level before one is
    /// presented to the server.
    ///
    /// A refresh that fails may still be a race lost to such a session, one
    /// that presented the same refresh token a moment earlier: the copies
    /// are levelled once more, and only a copy that is still spent is a
    /// failure.
    fn hand_out(&self, slug: &str, accounts: &mut [Stashed], live: &Live) -> Result<Grant> {
        let position = accounts.iter().position(|a| a.slug == slug).context("no such account")?;
        if !accounts[position].account.oauth.needs_refresh() {
            return Ok(grant_of(&accounts[position]));
        }
        reconcile(self.ctx, accounts, live)?;
        let provider = accounts[position].account.provider;
        let refreshed = freshen(self.ctx.apis(), provider, &accounts[position].account.oauth);
        match refreshed {
            Ok(Some(oauth)) => propagate(self.ctx, &mut accounts[position], &oauth, live)?,
            Ok(None) => {}
            Err(e) => {
                reconcile(self.ctx, accounts, live)?;
                if accounts[position].account.oauth.needs_refresh() {
                    return Err(e);
                }
            }
        }
        Ok(grant_of(&accounts[position]))
    }
}

fn grant_of(entry: &Stashed) -> Grant {
    Grant {
        provider: entry.account.provider,
        slug: entry.slug.clone(),
        email: entry.account.email.clone(),
        token: entry.account.oauth.access_token.clone(),
        account_id: entry.account.oauth.account_id().map(String::from),
    }
}

/// The account of `provider` a request pinned to `needle` goes out as. The
/// needle is read the way `ccs use` reads one, among that provider's accounts
/// only, so an address signed up with both providers names the right one.
/// Ambiguity is reported in `resolve`'s words; a needle naming nothing, in
/// the gateway's.
fn pinned(accounts: &[Stashed], provider: Provider, needle: &str) -> Result<String> {
    let mine: Vec<Stashed> =
        accounts.iter().filter(|a| a.account.provider == provider).cloned().collect();
    let lowered = needle.to_lowercase();
    let named = mine.iter().any(|a| {
        a.slug.starts_with(&lowered) || a.account.email.to_lowercase().starts_with(&lowered)
    });
    if !named {
        bail!("no such account for {provider}: {needle}");
    }
    Ok(stash::resolve(&mine, needle)?.slug.clone())
}

impl serve::Accounts for Desk<'_> {
    fn grant_model(
        &self,
        provider: Provider,
        model: Option<&str>,
        pin: Option<&str>,
        avoid: &[String],
    ) -> Result<Grant, String> {
        let routed = || -> Result<Grant> {
            let routing = crate::routing::Routing::read(self.ctx.stash.root())?;
            let rule = pin.is_none().then(|| model.and_then(|m| routing.matching(provider, m)));
            // A client that named its account gets that one, route or no route.
            let Some(rule) = rule.flatten() else {
                return self.grant(provider, pin, avoid).map_err(anyhow::Error::msg);
            };
            let mut accounts = self.ctx.stash.list()?;
            // Validate every target before sending; never silently use another provider.
            for slug in &rule.accounts {
                if !accounts.iter().any(|a| a.slug == *slug && a.account.provider == provider) {
                    bail!("route {} refers to missing {} account {}", rule.model, provider, slug);
                }
            }
            let slug = rule
                .accounts
                .iter()
                .find(|s| !avoid.contains(s))
                .context("all accounts assigned to this model are limited")?;
            let live = self.live(&accounts)?;
            self.hand_out(slug, &mut accounts, &live)
        };
        routed().map_err(|e| describe(&e))
    }

    fn grant(
        &self,
        provider: Provider,
        pin: Option<&str>,
        avoid: &[String],
    ) -> Result<Grant, String> {
        let granted = || -> Result<Grant> {
            let mut accounts = self.ctx.stash.list()?;
            let live = self.live(&accounts)?;
            let slug = match pin {
                Some(needle) => pinned(&accounts, provider, needle)?,
                None => self.choose(&accounts, provider, live.of(provider), avoid)?,
            };
            self.hand_out(&slug, &mut accounts, &live)
        };
        granted().map_err(|e| describe(&e))
    }

    fn stale(&self, grant: &Grant) -> Result<Option<Grant>, String> {
        let renewed = || -> Result<Option<Grant>> {
            let mut accounts = self.ctx.stash.list()?;
            let live = self.live(&accounts)?;
            reconcile(self.ctx, &mut accounts, &live)?;
            let entry =
                accounts.iter().find(|a| a.slug == grant.slug).context("no such account")?;
            Ok((entry.account.oauth.access_token != grant.token).then(|| grant_of(entry)))
        };
        renewed().map_err(|e| describe(&e))
    }
}

pub fn pick(ctx: &Ctx) -> Result<()> {
    let mut accounts = stashed(ctx)?;
    // Switching is done from inside the picker, which stays up for it, so
    // nothing is left over here to act on.
    choose(ctx, &mut accounts, Verb::Switch).map(|_| ())
}

/// Confine a session to one account: install that account's credentials in its
/// own pen and hand the process over to its client there.
///
/// Nothing outside the pen is touched, so every other session stays on the
/// account in use and a later `ccs use` leaves this one where it is.
pub fn pin(
    ctx: &Ctx,
    provider: Option<Provider>,
    needle: Option<&str>,
    args: &[String],
) -> Result<()> {
    let mut accounts = stashed(ctx)?;
    let live = identify_live(ctx, &accounts)?;
    reconcile(ctx, &mut accounts, &live)?;

    // Named outright, the account is taken at its word and the session starts
    // without a round trip; the picker is where usage is shopped for.
    let target = match needle {
        Some(needle) => match provider {
            Some(provider) => {
                let slug = pinned(&accounts, provider, needle)?;
                accounts
                    .iter()
                    .find(|a| a.slug == slug)
                    .context("resolved account missing")?
                    .clone()
            }
            None => stash::resolve(&accounts, needle)?.clone(),
        },
        None => {
            if let Some(provider) = provider {
                accounts.retain(|a| a.account.provider == provider);
                if accounts.is_empty() {
                    bail!("no accounts stashed for {provider}");
                }
            }
            let Some(chosen) = choose(ctx, &mut accounts, Verb::Launch)? else { return Ok(()) };
            chosen
        }
    };
    let pen = pen_for(ctx, &target, &live)?;
    let binary = match target.account.provider {
        Provider::Claude => {
            warn_overridden();
            claude_binary()
        }
        Provider::Codex => codex_binary(),
    };
    println!("{} is pinned to this session only", target.account.email);
    pen::launch(&pen, target.account.provider, &binary, args)
}

/// The account's pen, with its freshest credentials in place.
///
/// The pen is built before the credentials are settled so that folding them out
/// finds it, which is what leaves the pen and every other copy of the account on
/// the one token a refresh here has not spent.
fn pen_for(ctx: &Ctx, target: &Stashed, live: &Live) -> Result<PathBuf> {
    let provider = target.account.provider;
    let pen = match provider {
        Provider::Claude => pen::prepare(ctx.home, ctx.stash.root(), &target.slug)?,
        Provider::Codex => pen::prepare_codex(ctx.codex.dir(), ctx.stash.root(), &target.slug)?,
    };

    let mut entry = target.clone();
    let oauth = freshen(ctx.apis(), provider, &entry.account.oauth)?
        .unwrap_or_else(|| entry.account.oauth.clone());
    if provider == Provider::Codex {
        install_codex(&codex::Store::at(&pen, Some(&pen)), &oauth)?;
    }
    propagate(ctx, &mut entry, &oauth, live)?;
    Ok(pen)
}

/// The environment variables answering for the credentials right now. Set, they
/// decide the account whatever any credentials file holds — the live one and a
/// pen's alike, which makes them worth saying wherever a switch is reported.
fn overriding() -> Vec<&'static str> {
    creds::OVERRIDING.iter().copied().filter(|k| std::env::var_os(k).is_some()).collect()
}

/// Say so when the environment answers for the credentials.
fn warn_overridden() {
    let set = overriding();
    if set.is_empty() {
        return;
    }
    eprintln!(
        "ccs: {} takes precedence over the credentials file; a session that inherits it uses \
         that account instead",
        set.join(", ")
    );
}

/// The stash as the picker sees it: something to poll, and something to act on.
///
/// One collaborator rather than two closures because both halves reach the same
/// accounts, and only one of them can borrow them at a time.
struct Deck<'a, 'b> {
    ctx: &'a Ctx<'b>,
    accounts: &'a mut [Stashed],
    verb: Verb,
    /// The account most recently installed from inside the picker, so the
    /// terminal the picker was left in still says what happened in it.
    switched: Option<(Stashed, usize)>,
}

impl picker::Accounts for Deck<'_, '_> {
    fn edit_route(&mut self, provider: Option<Provider>) -> Result<Option<String>> {
        crate::route_picker::edit(self.ctx, provider)
    }

    fn poll(&mut self) -> Result<Table> {
        survey(self.ctx, self.accounts, Style::colored()).map(|(table, _)| table)
    }

    /// A launch is handed back because it takes the process itself; a switch is
    /// done here and now, leaving the picker up.
    ///
    /// The copies are brought into step first, exactly as `ccs use` does. A
    /// picker that stays up can switch twice, and between the two the account it
    /// left may have had its live tokens refreshed underneath it by the sessions
    /// that followed the first switch.
    fn act(&mut self, slug: &str) -> Result<Act> {
        if matches!(self.verb, Verb::Launch) {
            return Ok(Act::Handed);
        }
        let live = identify_live(self.ctx, self.accounts)?;
        reconcile(self.ctx, self.accounts, &live)?;

        let target = stash::resolve(self.accounts, slug)?.clone();
        let told = switch_to(self.ctx, self.accounts, &target)?;

        let said = match overriding().as_slice() {
            [] => format!("switched to {}{}", target.account.email, heard(told)),
            set => {
                format!("switched to {}, but {} overrides it", target.account.email, set.join(", "))
            }
        };
        self.switched = Some((target, told));
        Ok(Act::Installed(said))
    }
}

/// Put the accounts on screen and wait. A switch happens inside the picker,
/// which stays up for it; only an act the picker cannot survive comes back, so
/// what this returns is the account a launch still has to be given. The picker
/// owns the whole screen while it is up, so it is always drawn in colour.
fn choose(ctx: &Ctx, accounts: &mut [Stashed], verb: Verb) -> Result<Option<Stashed>> {
    let mut deck = Deck { ctx, accounts, verb, switched: None };
    let outcome = picker::run(&mut deck, verb);
    let switched = deck.switched.take();

    // Reported after the screen is down, and whether or not the picker itself
    // came back cleanly: the switch already happened on disk either way.
    if let Some((target, told)) = &switched {
        report_switch(target, *told);
    }
    let Outcome::Chose(slug) = outcome? else { return Ok(None) };
    Ok(accounts.iter().find(|a| a.slug == slug).cloned())
}

/// Every stashed account, refusing to go further when there are none.
fn stashed(ctx: &Ctx) -> Result<Vec<Stashed>> {
    let accounts = ctx.stash.list()?;
    if accounts.is_empty() {
        bail!("no accounts stashed yet; run `ccs add` and choose Claude Code or Codex");
    }
    validate_claude_credentials(&accounts)?;
    Ok(accounts)
}

fn validate_claude_credentials(accounts: &[Stashed]) -> Result<()> {
    for (i, first) in accounts.iter().enumerate() {
        if first.account.provider != Provider::Claude {
            continue;
        }
        for second in &accounts[i + 1..] {
            if second.account.provider == Provider::Claude
                && first.account.uuid != second.account.uuid
                && first.account.oauth.refresh_token == second.account.oauth.refresh_token
            {
                bail!(
                    "{} and {} share credentials but name different Claude accounts; run `ccs repair` before polling or switching",
                    first.slug,
                    second.slug
                );
            }
        }
    }
    Ok(())
}

// ── shared steps ────────────────────────────────────────────────────────────

/// Probe every account, write back anything that got refreshed, and lay the
/// results out as a table.
fn survey(ctx: &Ctx, accounts: &mut [Stashed], style: Style) -> Result<(Table, Live)> {
    let live = identify_live(ctx, accounts)?;
    reconcile(ctx, accounts, &live)?;
    let probes = probe_all(ctx, accounts);
    persist(ctx, accounts, &probes, &live)?;

    let entries = accounts
        .iter()
        .zip(&probes)
        .map(|(account, probe)| entry_of(account, probe, live.holds(account)))
        .collect();
    Ok((Table::build(entries, style), live))
}

/// Which stashed account each provider's live slot holds.
fn identify_live(ctx: &Ctx, accounts: &[Stashed]) -> Result<Live> {
    let installed = slots(ctx)?;
    let mut live = Live::default();
    for provider in Provider::ALL {
        live.set(provider, identify(ctx, accounts, provider, installed.of(provider)));
    }
    Ok(live)
}

/// Install `target` as the live credentials.
///
/// The outgoing account's tokens are folded back into the stash first: Claude
/// Code refreshes tokens in place, so the stashed copy goes stale the moment an
/// account is used. Capturing it on the way out is what keeps a stashed account
/// usable without a fresh login.
fn switch_to(ctx: &Ctx, accounts: &mut [Stashed], target: &Stashed) -> Result<usize> {
    if target.account.provider == Provider::Codex {
        return switch_codex(ctx, accounts, target);
    }
    let _guard = lock::acquire(ctx.creds.dir())?;
    let live = ctx.creds.read()?;
    if let Some(live) = &live {
        capture_outgoing(ctx, accounts, Provider::Claude, &live.oauth, &target.slug)?;
    }
    ctx.creds.write(&merged(live, &target.account.oauth))?;
    if pen::home_of(ctx.creds.dir()).is_some() {
        pen::set_account(ctx.creds.dir(), &target.slug)?;
    } else {
        ctx.stash.set_active(target.account.provider, &target.slug)?;
    }
    drop(_guard);
    let told = notify::broadcast(
        ctx.creds.dir(),
        ctx.stash.root(),
        Provider::Claude,
        "switch",
        &format!(
            "this session's account is now {} ({}). \
             The API prompt cache is per account, so the next request re-prefills everything.",
            target.account.email, target.slug
        ),
    );
    Ok(told)
}

/// What a switch leaves behind in the terminal it happened in. The picker says
/// its own piece in the footer while it is up; this is the record that outlives
/// the screen.
fn report_switch(target: &Stashed, told: usize) {
    println!("switched to {} ({})", target.account.email, target.slug);
    match target.account.provider {
        Provider::Claude => {
            println!("running sessions pick this up on their next request{}", heard(told));
            warn_overridden();
        }
        Provider::Codex => println!(
            "Codex login installed{}; verify with /status and resume if the session still shows the previous account",
            heard(told)
        ),
    }
}

/// How many subscribed sessions heard about it, for the tail of a report.
fn heard(told: usize) -> String {
    match told {
        0 => String::new(),
        1 => "; told 1 subscribed session".into(),
        n => format!("; told {n} subscribed sessions"),
    }
}

/// Install `target` as Codex's login and notify subscribers to that home.
fn switch_codex(ctx: &Ctx, accounts: &mut [Stashed], target: &Stashed) -> Result<usize> {
    std::fs::create_dir_all(ctx.codex.dir())
        .with_context(|| format!("creating {}", ctx.codex.dir().display()))?;
    let _guard = lock::acquire(ctx.codex.dir())?;
    let current = ctx.codex.read()?;
    if let Some(live) = current.as_ref().and_then(|f| f.oauth()) {
        capture_outgoing(ctx, accounts, Provider::Codex, &live, &target.slug)?;
    } else if current.as_ref().is_some_and(|f| f.is_api_key()) {
        // An API-key login is not an account the stash can hold, so it is
        // not captured on the way out; said, so it is not a surprise.
        eprintln!("ccs: {} held an API-key login; it is replaced", ctx.codex.describe());
    }
    ctx.codex.write(&current.unwrap_or_default().with(&target.account.oauth))?;
    if pen::codex_home_of(ctx.codex.dir())?.is_none() {
        ctx.stash.set_active(Provider::Codex, &target.slug)?;
    }
    drop(_guard);
    Ok(notify::broadcast(
        ctx.codex.dir(),
        ctx.stash.root(),
        Provider::Codex,
        "switch",
        &format!(
            "Codex account changed to {} ({}). Use /status to verify the running session's account; resume the session if it still shows the previous login.",
            target.account.email, target.slug
        ),
    ))
}

/// Fold the outgoing account's live credentials into its stash entry.
///
/// The stash is the only copy written here, and that is enough: every copy of
/// that account was brought into step before the switch began, so the live file
/// about to be replaced is the only one that can have moved since. It is also
/// the only write this can do — the caller holds the credential lock, which
/// installing anything would try to take again.
///
/// The entry in hand is moved onto those credentials too. Leaving it behind
/// would leave the caller holding a token the file it just wrote has superseded,
/// which is the whole failure this exists to prevent.
fn capture_outgoing(
    ctx: &Ctx,
    accounts: &mut [Stashed],
    provider: Provider,
    live: &Oauth,
    incoming: &str,
) -> Result<()> {
    let Some(slug) = identify(ctx, accounts, provider, Some(live)) else { return Ok(()) };
    if slug == incoming {
        return Ok(());
    }
    let Some(entry) = accounts.iter_mut().find(|a| a.slug == slug) else { return Ok(()) };
    entry.account.oauth = live.clone();
    ctx.stash.save(&slug, &entry.account)
}

/// Refuse to walk into an account with nothing left, unless told to.
fn guard_exhausted(entry: &Entry, force: bool) -> Result<()> {
    if force {
        return Ok(());
    }
    if entry.restrictions().is_empty() {
        return Ok(());
    }
    if !confirm(&render::question(entry, Verb::Switch))? {
        bail!("not switching; `ccs use <account> --force` overrides");
    }
    Ok(())
}

fn confirm(question: &str) -> Result<bool> {
    if !io::stdin().is_terminal() {
        return Ok(false);
    }
    print!("{question} [y/N] ");
    io::stdout().flush().context("prompting")?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer).context("reading answer")?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
}

// ── machine-readable output ─────────────────────────────────────────────────

#[derive(Serialize)]
struct View<'a> {
    provider: Provider,
    slug: &'a str,
    email: &'a str,
    plan: &'a str,
    active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
    /// When the limits were read, for a listing that did not read them now.
    #[serde(skip_serializing_if = "Option::is_none")]
    polled_at: Option<&'a str>,
    limits: &'a [Limit],
    #[serde(skip_serializing_if = "Option::is_none")]
    model_usage: Option<&'a std::collections::BTreeMap<String, ModelAvailability>>,
}

fn view<'a>(entry: &'a Entry, polled_at: Option<&'a str>) -> View<'a> {
    View {
        provider: entry.provider,
        slug: &entry.slug,
        email: &entry.email,
        plan: &entry.plan,
        active: entry.active,
        error: entry.usage.as_ref().err().map(String::as_str),
        polled_at,
        limits: entry.known(),
        model_usage: entry.models(),
    }
}

fn emit_json(table: &Table) -> Result<()> {
    let views: Vec<View> = table.entries().iter().map(|e| view(e, None)).collect();
    println!("{}", serde_json::to_string_pretty(&views)?);
    Ok(())
}

/// Lay one account and its probe out as a table entry. The single place that
/// mapping happens, so the picker, the table and the switch guard all agree.
fn entry_of(account: &Stashed, probe: &Probe, active: bool) -> Entry {
    Entry {
        provider: account.account.provider,
        slug: account.slug.clone(),
        email: account.account.email.clone(),
        plan: account.account.plan_label(),
        active,
        usage: probe.usage.clone(),
    }
}

fn describe(error: &anyhow::Error) -> String {
    error.chain().map(|c| c.to_string()).collect::<Vec<_>>().join(": ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limit;
    use crate::picker::Accounts as _;
    use std::fs;
    use std::path::Path;
    use std::sync::Mutex;

    /// Credentials held in memory rather than on disk, so a test can see what
    /// a command installed.
    struct Recorder {
        dir: PathBuf,
        held: Mutex<Option<CredsFile>>,
    }

    impl CredStore for Recorder {
        fn read(&self) -> Result<Option<CredsFile>> {
            Ok(self.held.lock().expect("recorder").clone())
        }

        fn write(&self, creds: &CredsFile) -> Result<()> {
            *self.held.lock().expect("recorder") = Some(creds.clone());
            Ok(())
        }

        fn dir(&self) -> &Path {
            &self.dir
        }

        fn describe(&self) -> String {
            format!("{} (held in memory)", self.dir.display())
        }
    }

    /// Credentials identified by their refresh token, expiring when told to.
    /// The expiry is what orders the copies of one account, so a test that
    /// cares which copy wins sets it deliberately.
    fn oauth(refresh_token: &str, expires_at: i64) -> Oauth {
        Oauth {
            access_token: format!("access-{refresh_token}"),
            refresh_token: refresh_token.to_string(),
            expires_at,
            scopes: vec![],
            subscription_type: None,
            extra: serde_json::Map::new(),
        }
    }

    /// Codex's login held in memory, as `Recorder` holds Claude's.
    struct CodexRecorder {
        dir: PathBuf,
        held: Mutex<Option<codex::AuthFile>>,
    }

    impl codex::Creds for CodexRecorder {
        fn read(&self) -> Result<Option<codex::AuthFile>> {
            Ok(self.held.lock().expect("recorder").clone())
        }

        fn write(&self, file: &codex::AuthFile) -> Result<()> {
            *self.held.lock().expect("recorder") = Some(file.clone());
            Ok(())
        }

        fn dir(&self) -> &Path {
            &self.dir
        }

        fn describe(&self) -> String {
            "the codex recorder".into()
        }
    }

    /// A Codex account's credentials: the pair, plus the identity token and
    /// account id every Codex request carries.
    fn codex_oauth(refresh_token: &str, expires_at: i64, email: &str) -> Oauth {
        let mut oauth = oauth(refresh_token, expires_at);
        let claims = serde_json::json!({
            "email": email,
            "https://api.openai.com/auth": {"chatgpt_account_id": format!("acct-{email}"), "chatgpt_plan_type": "pro"}
        });
        let token = format!("h.{}.s", codex::base64url(claims.to_string().as_bytes()));
        // Codex access tokens are JWTs, and the expiry is read off them.
        let access = serde_json::json!({"exp": expires_at / 1000, "jti": refresh_token});
        oauth.access_token = format!("h.{}.s", codex::base64url(access.to_string().as_bytes()));
        oauth.extra.insert("idToken".into(), serde_json::Value::String(token));
        oauth.extra.insert("accountId".into(), serde_json::Value::String(format!("acct-{email}")));
        oauth
    }

    /// The live slots with a Claude account in the Claude one.
    fn claude_live(slug: &str) -> Live {
        Live { claude: Some(slug.to_string()), codex: None }
    }

    fn codex_stashed(slug: &str, oauth: Oauth) -> Stashed {
        let mut entry = stashed(slug, oauth);
        entry.account.provider = Provider::Codex;
        entry.account.uuid = entry.account.oauth.account_id().expect("account id").to_string();
        entry.account.email =
            codex::identity(entry.account.oauth.id_token().unwrap()).unwrap().email;
        entry
    }

    /// A configuration directory with a stash in it, private to one test so
    /// concurrently running tests never share a path.
    struct Fixture {
        dir: PathBuf,
        creds: Recorder,
        codex: CodexRecorder,
        codex_api: codex::Client,
        stash: Stash,
        usage: usage::Cache,
        api: Api,
        home: pen::Home,
        /// Accounts a gateway desk may fall over to.
        pool: Vec<String>,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("ccs-cmd-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("config directory");
            let stash = Stash::open(&dir).expect("stash");
            Self {
                creds: Recorder { dir: dir.clone(), held: Mutex::new(None) },
                codex: CodexRecorder { dir: dir.join("codex"), held: Mutex::new(None) },
                codex_api: codex::Client::new(),
                usage: usage::Cache::open(stash.root()).expect("usage cache"),
                stash,
                api: Api::new(),
                home: pen::Home { config: dir.clone(), global: dir.join(".claude.json") },
                dir,
                pool: Vec::new(),
            }
        }

        fn ctx(&self) -> Ctx<'_> {
            Ctx {
                creds: &self.creds,
                // A test writes credentials where it can look at them, never
                // into the keychain of whoever is running it.
                backend: Backend::File,
                stash: &self.stash,
                usage: &self.usage,
                api: &self.api,
                codex: &self.codex,
                codex_api: &self.codex_api,
                home: &self.home,
            }
        }

        /// The refresh token in Codex's live slot, if anything is there.
        fn codex_live(&self) -> Option<String> {
            self.codex.read().expect("read codex").and_then(|f| f.oauth()).map(|o| o.refresh_token)
        }

        /// Put credentials where a session confined to `slug` would keep them.
        fn pin(&self, slug: &str, oauth: &Oauth) {
            pen::prepare(&self.home, self.stash.root(), slug).expect("pen");
            self.pen(slug).write(&CredsFile::new(oauth.clone())).expect("pen credentials");
        }

        fn pen(&self, slug: &str) -> Box<dyn CredStore> {
            Backend::File.confined(&pen::at(self.stash.root(), slug))
        }

        /// The usage reading recorded for `slug`, if one was.
        fn reading(&self, slug: &str) -> Option<usage::Reading> {
            let raw =
                fs::read(self.stash.root().join("usage").join(format!("{slug}.json"))).ok()?;
            Some(serde_json::from_slice(&raw).expect("parses"))
        }

        /// The refresh token each copy of `slug` is holding: stash, live, pen.
        fn tokens(&self, slug: &str) -> (String, Option<String>, Option<String>) {
            let accounts = self.stash.list().expect("list");
            let stashed = accounts.iter().find(|s| s.slug == slug).expect("stash entry");
            let token = |file: Option<CredsFile>| file.map(|f| f.oauth.refresh_token);
            (
                stashed.account.oauth.refresh_token.clone(),
                token(self.creds.read().expect("read live")),
                token(self.pen(slug).read().expect("read pen")),
            )
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn stashed(slug: &str, oauth: Oauth) -> Stashed {
        Stashed {
            slug: slug.into(),
            account: Account {
                provider: Provider::Claude,
                email: format!("{slug}@example.com"),
                uuid: "u".into(),
                plan: None,
                rate_limit_tier: None,
                added_at: "2026-01-01T00:00:00Z".into(),
                oauth,
            },
        }
    }

    /// A rotated refresh token has to reach every copy at once: the stash, the
    /// live credentials, the pen a session is confined in, and the in-memory
    /// list the caller goes on to install from. A copy left on the superseded
    /// token buys nothing, and a session handed one has to log in again.
    #[test]
    fn a_refresh_reaches_every_copy_of_the_account_it_refreshed() {
        let fixture = Fixture::new("persist");
        fixture.pin("work", &oauth("superseded", 1));

        let mut accounts = vec![stashed("work", oauth("superseded", 1))];
        let probes = vec![Probe {
            slug: "work".into(),
            refreshed: Some(oauth("rotated", 2)),
            usage: Ok(vec![].into()),
        }];
        persist(&fixture.ctx(), &mut accounts, &probes, &claude_live("work")).expect("persist");

        assert_eq!(accounts[0].account.oauth.refresh_token, "rotated", "the list the caller holds");
        let (stashed, live, pen) = fixture.tokens("work");
        assert_eq!(stashed, "rotated", "the stash");
        assert_eq!(live.as_deref(), Some("rotated"), "the live credentials");
        assert_eq!(pen.as_deref(), Some("rotated"), "the pen");
    }

    #[test]
    fn the_furthest_expiry_is_the_copy_that_was_minted_last() {
        let copies = [oauth("spent", 10), oauth("current", 30), oauth("older", 20)];
        assert_eq!(newest(copies).expect("a copy").refresh_token, "current");
    }

    #[test]
    fn newest_of_nothing_is_nothing() {
        assert!(newest([]).is_none());
    }

    /// The live session refreshes the credentials this tool installed for it,
    /// which spends the token the stash is holding. Reading the stash entry
    /// afterwards is reading a token the server will refuse.
    #[test]
    fn credentials_the_live_session_refreshed_are_taken_back_into_the_stash() {
        let fixture = Fixture::new("live");
        fixture.creds.write(&CredsFile::new(oauth("rotated", 2))).expect("live credentials");

        let mut accounts = vec![stashed("work", oauth("superseded", 1))];
        reconcile(&fixture.ctx(), &mut accounts, &claude_live("work")).expect("reconcile");

        assert_eq!(accounts[0].account.oauth.refresh_token, "rotated", "the list the caller holds");
        assert_eq!(fixture.tokens("work").0, "rotated", "the stash");
    }

    /// Same for a pinned session: it refreshes inside its pen, and nothing else
    /// hears about it.
    #[test]
    fn credentials_a_pinned_session_refreshed_are_taken_back_into_the_stash() {
        let fixture = Fixture::new("pen");
        fixture.pin("work", &oauth("rotated", 2));
        fixture.creds.write(&CredsFile::new(oauth("elsewhere", 5))).expect("live credentials");

        let mut accounts = vec![stashed("work", oauth("superseded", 1))];
        reconcile_with(&fixture.ctx(), &mut accounts, &claude_live("other"), &|_| Ok("u".into()))
            .expect("reconcile");

        assert_eq!(accounts[0].account.oauth.refresh_token, "rotated", "the list the caller holds");
        let (stashed, live, _) = fixture.tokens("work");
        assert_eq!(stashed, "rotated", "the stash");
        assert_eq!(live.as_deref(), Some("elsewhere"), "another account's live credentials");
    }

    #[test]
    fn repinning_a_claude_session_never_assigns_its_new_login_to_the_old_account() {
        let fixture = Fixture::new("claude-repin");
        let hong = stashed("hong", oauth("r-hong", LATER));
        let mut robin = stashed("robin", oauth("r-robin", LATER));
        robin.account.uuid = "robin-uuid".into();
        fixture.stash.save("hong", &hong.account).unwrap();
        fixture.stash.save("robin", &robin.account).unwrap();
        fixture.stash.set_active(Provider::Claude, "hong").unwrap();
        fixture.creds.write(&CredsFile::new(hong.account.oauth.clone())).unwrap();
        let path = pen_for(&fixture.ctx(), &hong, &claude_live("hong")).unwrap();
        let pinned_store = Backend::File.confined(&path);
        let pinned = Ctx { creds: pinned_store.as_ref(), ..fixture.ctx() };
        let mut accounts = fixture.stash.list().unwrap();

        switch_to(&pinned, &mut accounts, &robin).unwrap();
        assert_eq!(fixture.stash.active(Provider::Claude).as_deref(), Some("hong"));
        let refreshed = oauth("r-robin-new", LATER + 1000);
        pinned_store.write(&CredsFile::new(refreshed)).unwrap();
        let live = identify_live(&fixture.ctx(), &accounts).unwrap();
        reconcile_with(&fixture.ctx(), &mut accounts, &live, &|_| Ok("robin-uuid".into())).unwrap();

        assert_eq!(fixture.tokens("hong").0, "r-hong");
        assert_eq!(fixture.tokens("robin").0, "r-robin-new");
        assert_eq!(fixture.tokens("hong").1.as_deref(), Some("r-hong"));
    }

    #[test]
    fn listing_refuses_two_claude_identities_with_one_refresh_token() {
        let fixture = Fixture::new("duplicate-claude-token");
        let hong = stashed("hong", oauth("shared", LATER));
        let mut robin = stashed("robin", oauth("shared", LATER));
        robin.account.uuid = "robin-uuid".into();
        fixture.stash.save("hong", &hong.account).unwrap();
        fixture.stash.save("robin", &robin.account).unwrap();

        assert!(
            readings(&fixture.ctx())
                .err()
                .expect("must refuse")
                .to_string()
                .contains("share credentials")
        );
    }

    #[test]
    fn reconciliation_repairs_a_pen_marker_using_credential_identity() {
        let fixture = Fixture::new("wrong-claude-pen");
        let hong = stashed("hong", oauth("r-hong", LATER));
        let mut robin = stashed("robin", oauth("r-robin", LATER + 1000));
        robin.account.uuid = "robin-uuid".into();
        fixture.stash.save("hong", &hong.account).unwrap();
        fixture.stash.save("robin", &robin.account).unwrap();
        fixture.pin("hong", &robin.account.oauth);
        let mut accounts = fixture.stash.list().unwrap();

        reconcile_with(&fixture.ctx(), &mut accounts, &Live::default(), &|_| {
            Ok("robin-uuid".into())
        })
        .unwrap();
        assert_eq!(fixture.tokens("hong").0, "r-hong");
        assert_eq!(fixture.tokens("robin").0, "r-robin");
        assert_eq!(
            pen::account_of(&pen::at(fixture.stash.root(), "hong")).as_deref(),
            Some("robin")
        );
    }

    #[test]
    fn a_rotated_robin_token_in_hongs_pen_never_replaces_hongs_stash() {
        let fixture = Fixture::new("rotated-wrong-pen");
        let hong = stashed("hong", oauth("r-hong", 10));
        let mut robin = stashed("robin", oauth("r-robin", 10));
        robin.account.uuid = "robin-uuid".into();
        fixture.stash.save("hong", &hong.account).unwrap();
        fixture.stash.save("robin", &robin.account).unwrap();
        fixture.pin("hong", &oauth("r-robin-rotated", 30));
        let mut accounts = fixture.stash.list().unwrap();

        reconcile_with(&fixture.ctx(), &mut accounts, &Live::default(), &|_| {
            Ok("robin-uuid".into())
        })
        .unwrap();

        assert_eq!(fixture.tokens("hong").0, "r-hong");
        assert_eq!(fixture.tokens("robin").0, "r-robin-rotated");
        assert_eq!(
            pen::account_of(&pen::at(fixture.stash.root(), "hong")).as_deref(),
            Some("robin")
        );
    }

    #[test]
    fn repair_restores_a_corrupted_claude_slot_without_a_login() {
        let fixture = Fixture::new("repair-duplicate");
        let mut hong = stashed("hong", oauth("r-hong", 30));
        let mut robin = stashed("robin", oauth("r-robin", 20));
        robin.account.uuid = "robin-uuid".into();
        fixture.pin("hong", &hong.account.oauth);
        fixture.pin("robin", &robin.account.oauth);
        hong.account.oauth = robin.account.oauth.clone();
        fixture.stash.save("hong", &hong.account).unwrap();
        fixture.stash.save("robin", &robin.account).unwrap();

        let repaired = repair_with(&fixture.ctx(), &|oauth| {
            Ok(if oauth.refresh_token == "r-hong" { "u" } else { "robin-uuid" }.into())
        })
        .unwrap();

        assert_eq!(repaired, vec!["hong"]);
        assert_eq!(fixture.tokens("hong").0, "r-hong");
        assert_eq!(fixture.tokens("robin").0, "r-robin");
        assert!(fixture.stash.root().join("repair-backups").read_dir().unwrap().next().is_some());
    }

    /// The newest copy goes to the laggards whichever copy it is, so an account
    /// refreshed here while a session was pinned to it leaves no dead pen.
    #[test]
    fn the_newest_copy_reaches_the_ones_that_fell_behind() {
        let fixture = Fixture::new("laggard");
        fixture.pin("work", &oauth("superseded", 1));

        let mut accounts = vec![stashed("work", oauth("rotated", 2))];
        reconcile_with(&fixture.ctx(), &mut accounts, &claude_live("work"), &|_| Ok("u".into()))
            .expect("reconcile");

        let (stashed, live, pen) = fixture.tokens("work");
        assert_eq!(stashed, "rotated", "the stash");
        assert_eq!(live.as_deref(), Some("rotated"), "the live credentials");
        assert_eq!(pen.as_deref(), Some("rotated"), "the pen");
    }

    #[test]
    fn copies_that_already_agree_are_left_alone() {
        let fixture = Fixture::new("agree");
        fixture.pin("work", &oauth("current", 2));

        let mut accounts = vec![stashed("work", oauth("current", 2))];
        reconcile(&fixture.ctx(), &mut accounts, &claude_live("work")).expect("reconcile");

        assert!(
            fixture.creds.read().expect("read").is_none(),
            "nothing to install, so nothing was"
        );
    }

    /// The picker's view of a stash, wired the way `choose` wires it.
    fn deck<'a, 'b>(ctx: &'a Ctx<'b>, accounts: &'a mut [Stashed], verb: Verb) -> Deck<'a, 'b> {
        Deck { ctx, accounts, verb, switched: None }
    }

    fn stash_all(fixture: &Fixture, accounts: &[Stashed]) {
        for entry in accounts {
            fixture.stash.save(&entry.slug, &entry.account).expect("stash");
        }
    }

    #[test]
    fn a_switch_is_made_from_inside_the_picker_rather_than_handed_back() {
        let fixture = Fixture::new("installed");
        let mut accounts = vec![stashed("a", oauth("a1", 1)), stashed("b", oauth("b1", 1))];
        stash_all(&fixture, &accounts);

        let ctx = fixture.ctx();
        let mut deck = deck(&ctx, &mut accounts, Verb::Switch);
        let Act::Installed(said) = deck.act("b").expect("switches") else {
            panic!("a switch should leave the picker up")
        };

        assert!(said.contains("b@example.com"), "{said}");
        assert_eq!(deck.switched.expect("recorded for the terminal").0.slug, "b");
        assert_eq!(fixture.tokens("b").1.as_deref(), Some("b1"));
        assert_eq!(fixture.stash.active(Provider::Claude).as_deref(), Some("b"));
    }

    #[test]
    fn a_launch_leaves_the_picker_and_installs_nothing() {
        let fixture = Fixture::new("handed");
        let mut accounts = vec![stashed("a", oauth("a1", 1))];
        stash_all(&fixture, &accounts);

        let ctx = fixture.ctx();
        let mut deck = deck(&ctx, &mut accounts, Verb::Launch);
        assert!(matches!(deck.act("a").expect("hands back"), Act::Handed));
        assert!(deck.switched.is_none());
        assert!(fixture.creds.read().expect("read").is_none(), "a launch installs nothing");
    }

    #[test]
    fn switching_away_and_back_without_leaving_the_picker_installs_the_live_token() {
        let fixture = Fixture::new("round-trip");
        let mut accounts = vec![stashed("a", oauth("a-old", 1)), stashed("b", oauth("b1", 1))];
        stash_all(&fixture, &accounts);
        fixture.stash.set_active(Provider::Claude, "a").expect("active");
        // A session on `a` refreshed the live credentials after the picker last
        // polled, so what the picker is holding for `a` is already spent.
        fixture.creds.write(&CredsFile::new(oauth("a-new", 2))).expect("live");

        let ctx = fixture.ctx();
        let mut deck = deck(&ctx, &mut accounts, Verb::Switch);
        deck.act("b").expect("switches to b");
        deck.act("a").expect("switches back to a");

        let installed = fixture.creds.read().expect("read").expect("installed");
        assert_eq!(
            installed.oauth.refresh_token, "a-new",
            "switching twice in one picker put back a token the first switch had superseded"
        );
    }

    #[test]
    fn cached_usage_does_not_refresh_an_expired_access_token_or_poll_again() {
        let fixture = Fixture::new("cached-probe");
        let account = stashed("a", oauth("expired", 0));
        fixture.usage.record("a", &vec![limit!("session", 12.0)].into()).unwrap();
        let result = probe(&fixture.ctx(), &account);
        assert!(result.refreshed.is_none());
        assert_eq!(result.usage.unwrap().limits[0].percent, 12.0);
    }

    /// A reader that cannot afford a poll — a menu bar repainting every few
    /// seconds — takes what the last poll wrote down, and is told how old it
    /// is; an account nothing has polled yet is said to be unread, not broken.
    #[test]
    fn a_cached_listing_is_the_last_readings_without_a_poll() {
        let fixture = Fixture::new("cached");
        for slug in ["a", "b"] {
            let entry = stashed(slug, oauth(&format!("r-{slug}"), 0));
            fixture.stash.save(&entry.slug, &entry.account).expect("stash");
        }
        fixture.stash.set_active(Provider::Claude, "b").expect("active");
        fixture.usage.record("a", &vec![limit!("session", 42.0)].into()).expect("records");

        let entries = readings(&fixture.ctx()).expect("lists");

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].entry.slug, "a");
        assert_eq!(entries[0].entry.known()[0].percent, 42.0);
        assert!(entries[0].polled_at.is_some());
        assert!(!entries[0].entry.active);
        assert!(entries[1].entry.active);
        assert!(entries[1].entry.usage.as_ref().unwrap_err().contains("not polled"));
        assert!(entries[1].polled_at.is_none());
    }

    #[test]
    fn forgetting_an_account_takes_its_usage_reading_with_it() {
        let fixture = Fixture::new("forgotten");
        let entry = stashed("gone", oauth("r", 0));
        fixture.stash.save(&entry.slug, &entry.account).expect("stash");
        fixture.usage.record("gone", &vec![limit!("session", 5.0)].into()).expect("records");

        remove(&fixture.ctx(), "gone").expect("removes");
        assert!(fixture.reading("gone").is_none());
    }

    // ── a second provider ───────────────────────────────────────────────────

    /// Codex has a live slot of its own; installing a Codex account fills it
    /// and touches nothing of Claude's, pointers included.
    #[test]
    fn switching_to_a_codex_account_fills_codex_own_slot_and_leaves_claude_alone() {
        let fixture = Fixture::new("codex-switch");
        fixture.creds.write(&CredsFile::new(oauth("r-a", LATER))).expect("claude live");
        let claude = stashed("a", oauth("r-a", LATER));
        let gpt = codex_stashed("g", codex_oauth("r-g", LATER, "g@x"));
        fixture.stash.save("a", &claude.account).expect("save");
        fixture.stash.save("g", &gpt.account).expect("save");
        fixture.stash.set_active(Provider::Claude, "a").expect("active");
        let mut accounts = fixture.stash.list().expect("list");

        switch_to(&fixture.ctx(), &mut accounts, &gpt).expect("switches");

        assert_eq!(fixture.codex_live().as_deref(), Some("r-g"));
        let file = fixture.codex.read().expect("read").expect("a file");
        assert_eq!(file.tokens.as_ref().map(|t| t.account_id.as_str()), Some("acct-g@x"));
        assert_eq!(fixture.tokens("a").1.as_deref(), Some("r-a"));
        assert_eq!(fixture.stash.active(Provider::Claude).as_deref(), Some("a"));
        assert_eq!(fixture.stash.active(Provider::Codex).as_deref(), Some("g"));
    }

    /// Codex CLI refreshes the login it holds; the account being left has
    /// its stash entry brought up to what the file held on the way out.
    #[test]
    fn switching_codex_captures_the_outgoing_login_into_its_stash_entry() {
        let fixture = Fixture::new("codex-capture");
        let h = codex_stashed("h", codex_oauth("r-h", LATER, "h@x"));
        let g = codex_stashed("g", codex_oauth("r-g", LATER, "g@x"));
        fixture.stash.save("h", &h.account).expect("save");
        fixture.stash.save("g", &g.account).expect("save");
        let live = codex::AuthFile::default().with(&codex_oauth("r-h-2", LATER + 1, "h@x"));
        fixture.codex.write(&live).expect("codex live");
        fixture.stash.set_active(Provider::Codex, "h").expect("active");
        let mut accounts = fixture.stash.list().expect("list");

        switch_to(&fixture.ctx(), &mut accounts, &g).expect("switches");

        assert_eq!(fixture.tokens("h").0, "r-h-2");
        assert_eq!(fixture.codex_live().as_deref(), Some("r-g"));
    }

    #[test]
    fn reconciling_levels_a_codex_account_against_its_live_file() {
        let fixture = Fixture::new("codex-reconcile");
        let g = codex_stashed("g", codex_oauth("r-g", 1, "g@x"));
        fixture.stash.save("g", &g.account).expect("save");
        let live = codex::AuthFile::default().with(&codex_oauth("r-g-2", LATER, "g@x"));
        fixture.codex.write(&live).expect("codex live");
        // Every switch writes the pointer; a rotated token is what it is for.
        fixture.stash.set_active(Provider::Codex, "g").expect("active");
        let mut accounts = fixture.stash.list().expect("list");

        let ctx = fixture.ctx();
        let live = identify_live(&ctx, &accounts).expect("identifies");
        assert_eq!(live.of(Provider::Codex), Some("g"));
        assert_eq!(live.of(Provider::Claude), None);
        reconcile(&ctx, &mut accounts, &live).expect("reconciles");

        assert_eq!(fixture.tokens("g").0, "r-g-2");
        assert_eq!(accounts[0].account.oauth.refresh_token, "r-g-2");
    }

    #[test]
    fn a_codex_account_is_recorded_under_the_name_its_token_gives() {
        let fixture = Fixture::new("codex-record");
        let recorded = record(
            &fixture.ctx(),
            Provider::Codex,
            codex_oauth("r-g", LATER, "you@x.com"),
            None,
            Installed::Yes,
        )
        .expect("records");

        assert_eq!(recorded.stashed.slug, "you_at_x.com");
        assert_eq!(recorded.stashed.account.provider, Provider::Codex);
        assert_eq!(recorded.stashed.account.email, "you@x.com");
        assert_eq!(recorded.stashed.account.uuid, "acct-you@x.com");
        assert_eq!(recorded.stashed.account.plan_label(), "codex pro");
        assert_eq!(fixture.stash.active(Provider::Codex).as_deref(), Some("you_at_x.com"));
        assert_eq!(fixture.stash.active(Provider::Claude), None);
    }

    /// The same email signed up with both providers gets the provider's name
    /// in front of its slug, and a name given outright never lands on the
    /// other provider's account — that would spend its refresh token.
    #[test]
    fn a_slug_the_other_provider_holds_is_prefixed_or_refused() {
        let fixture = Fixture::new("codex-collide");
        let claude = stashed("you_at_x.com", oauth("r-a", LATER));
        fixture.stash.save("you_at_x.com", &claude.account).expect("save");

        let derived = record(
            &fixture.ctx(),
            Provider::Codex,
            codex_oauth("r-g", LATER, "you@x.com"),
            None,
            Installed::No,
        )
        .expect("records");
        assert_eq!(derived.stashed.slug, "codex-you_at_x.com");

        let error = record(
            &fixture.ctx(),
            Provider::Codex,
            codex_oauth("r-g2", LATER, "other@x.com"),
            Some("you_at_x.com"),
            Installed::No,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("claude"), "{error}");
        assert_eq!(fixture.tokens("you_at_x.com").0, "r-a");
    }

    /// A pool can name accounts of both providers; a Codex request only
    /// ever falls over to a Codex account.
    #[test]
    fn the_desk_keeps_a_mixed_pool_to_the_providers_own_accounts() {
        let fixture = Fixture::new("desk-mixed");
        for (slug, provider) in [("a", Provider::Claude), ("b", Provider::Claude)] {
            let mut entry = stashed(slug, oauth(&format!("r-{slug}"), LATER));
            entry.account.provider = provider;
            fixture.stash.save(slug, &entry.account).expect("save");
        }
        for slug in ["g", "h"] {
            let entry =
                codex_stashed(slug, codex_oauth(&format!("r-{slug}"), LATER, &format!("{slug}@x")));
            fixture.stash.save(slug, &entry.account).expect("save");
        }
        fixture.stash.set_active(Provider::Claude, "a").expect("active");
        fixture.stash.set_active(Provider::Codex, "g").expect("active");
        let pool = ["b", "h", "a", "g"].map(String::from).to_vec();
        let ctx = fixture.ctx();
        let desk = Desk { ctx: &ctx, pool: &pool };

        let codex = desk.grant(Provider::Codex, None, &[]).expect("grants");
        assert_eq!((codex.slug.as_str(), codex.account_id.as_deref()), ("g", Some("acct-g@x")));
        assert_eq!(desk.grant(Provider::Codex, None, &["g".into()]).expect("falls over").slug, "h");
        assert_eq!(
            desk.grant(Provider::Claude, None, &["a".into()]).expect("falls over").slug,
            "b"
        );
    }

    #[test]
    fn a_codex_pin_has_its_own_credentials_and_leaves_live_slots_alone() {
        let fixture = Fixture::new("codex-pin");
        let g = codex_stashed("g", codex_oauth("r-g", LATER, "g@x"));
        fixture.stash.save("g", &g.account).expect("save");
        let path = pen_for(&fixture.ctx(), &g, &Live::default()).expect("prepares");
        let store = codex::Store::at(&path, Some(&path));
        assert_eq!(store.read().unwrap().unwrap().oauth().unwrap().refresh_token, "r-g");
        assert!(fixture.codex_live().is_none());
        assert!(fixture.creds.read().unwrap().is_none());
        assert!(fixture.stash.active(Provider::Codex).is_none());
    }

    #[test]
    fn codex_pin_refreshes_are_kept_with_the_account_after_repinning() {
        let fixture = Fixture::new("codex-repin");
        let g = codex_stashed("g", codex_oauth("r-g", LATER, "g@x"));
        let h = codex_stashed("h", codex_oauth("r-h", LATER, "h@x"));
        fixture.stash.save("g", &g.account).unwrap();
        fixture.stash.save("h", &h.account).unwrap();
        fixture.codex.write(&codex::AuthFile::default().with(&g.account.oauth)).unwrap();
        fixture.stash.set_active(Provider::Codex, "g").unwrap();
        let path = pen_for(&fixture.ctx(), &g, &Live::default()).unwrap();
        let store = codex::Store::at(&path, Some(&path));
        let pinned = Ctx { codex: &store, ..fixture.ctx() };
        let mut accounts = fixture.stash.list().unwrap();
        switch_to(&pinned, &mut accounts, &h).unwrap();
        assert_eq!(fixture.stash.active(Provider::Codex).as_deref(), Some("g"));
        assert_eq!(fixture.codex_live().as_deref(), Some("r-g"));
        assert_eq!(identify_live(&pinned, &accounts).unwrap().codex.as_deref(), Some("h"));
        let refreshed = codex_oauth("r-h-new", LATER + 1000, "h@x");
        store.write(&codex::AuthFile::default().with(&refreshed)).unwrap();
        let live = identify_live(&fixture.ctx(), &accounts).unwrap();
        reconcile(&fixture.ctx(), &mut accounts, &live).unwrap();
        assert_eq!(fixture.tokens("h").0, "r-h-new");
        assert_eq!(fixture.tokens("g").0, "r-g");
        assert_eq!(fixture.codex_live().as_deref(), Some("r-g"));
    }

    #[test]
    fn polling_a_codex_account_propagates_its_refresh_to_the_pin() {
        let fixture = Fixture::new("codex-pin-propagate");
        let g = codex_stashed("g", codex_oauth("r-g", LATER, "g@x"));
        fixture.stash.save("g", &g.account).unwrap();
        let path = pen_for(&fixture.ctx(), &g, &Live::default()).unwrap();
        let mut accounts = fixture.stash.list().unwrap();
        let probe = Probe {
            slug: "g".into(),
            refreshed: Some(codex_oauth("r-new", LATER + 1, "g@x")),
            usage: Err("poll failed".into()),
        };
        persist(&fixture.ctx(), &mut accounts, &[probe], &Live::default()).unwrap();
        let store = codex::Store::at(&path, Some(&path));
        assert_eq!(store.read().unwrap().unwrap().oauth().unwrap().refresh_token, "r-new");
        assert_eq!(fixture.tokens("g").0, "r-new");
        assert!(fixture.codex_live().is_none());
    }

    #[test]
    fn refreshing_inside_a_codex_pin_updates_the_matching_global_login() {
        let fixture = Fixture::new("codex-pin-to-global");
        let g = codex_stashed("g", codex_oauth("r-g", LATER, "g@x"));
        fixture.stash.save("g", &g.account).unwrap();
        let global = codex::Store::at(&fixture.dir, Some(fixture.codex.dir()));
        global.write(&codex::AuthFile::default().with(&g.account.oauth)).unwrap();
        let path = pen_for(&fixture.ctx(), &g, &Live::default()).unwrap();
        let store = codex::Store::at(&path, Some(&path));
        let pinned = Ctx { codex: &store, ..fixture.ctx() };
        let mut accounts = fixture.stash.list().unwrap();
        let live = identify_live(&pinned, &accounts).unwrap();
        let probe = Probe {
            slug: "g".into(),
            refreshed: Some(codex_oauth("r-new", LATER + 1000, "g@x")),
            usage: Ok(Vec::new().into()),
        };
        persist(&pinned, &mut accounts, &[probe], &live).unwrap();
        assert_eq!(global.read().unwrap().unwrap().oauth().unwrap().refresh_token, "r-new");
        assert_eq!(store.read().unwrap().unwrap().oauth().unwrap().refresh_token, "r-new");
    }

    #[test]
    fn removing_a_codex_account_clears_only_pins_currently_holding_that_account() {
        let fixture = Fixture::new("codex-remove-repin");
        let g = codex_stashed("g", codex_oauth("r-g", LATER, "g@x"));
        let h = codex_stashed("h", codex_oauth("r-h", LATER, "h@x"));
        fixture.stash.save("g", &g.account).unwrap();
        fixture.stash.save("h", &h.account).unwrap();
        let path = pen_for(&fixture.ctx(), &g, &Live::default()).unwrap();
        let store = codex::Store::at(&path, Some(&path));
        store.write(&codex::AuthFile::default().with(&h.account.oauth)).unwrap();
        fs::write(path.join("history.jsonl"), "conversation").unwrap();
        remove(&fixture.ctx(), "g").unwrap();
        assert_eq!(store.read().unwrap().unwrap().oauth().unwrap().refresh_token, "r-h");
        remove(&fixture.ctx(), "h").unwrap();
        assert!(store.read().unwrap().is_none());
        assert_eq!(fs::read_to_string(path.join("history.jsonl")).unwrap(), "conversation");
    }

    #[test]
    fn cached_codex_status_reads_the_login_even_when_the_global_pointer_differs() {
        let fixture = Fixture::new("cached-codex-status");
        let g = codex_stashed("g", codex_oauth("expired", 1, "g@x"));
        fixture.stash.save("g", &g.account).unwrap();
        fixture.stash.set_active(Provider::Codex, "other").unwrap();
        fixture.codex.write(&codex::AuthFile::default().with(&g.account.oauth)).unwrap();
        fixture.usage.record("g", &vec![limit!("session", 42.0)].into()).unwrap();
        // An explicit refresh must reuse the shared recent reading too, without OAuth calls.
        let refreshed = status_entries(&fixture.ctx(), false, Some(Provider::Codex)).unwrap();
        assert_eq!(refreshed[0].entry.known()[0].percent, 42.0);
        let entries = status_entries(&fixture.ctx(), true, Some(Provider::Codex)).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].entry.slug, "g");
        assert_eq!(entries[0].entry.known()[0].percent, 42.0);
        assert!(entries[0].polled_at.is_some());
        assert_eq!(fixture.codex_live().as_deref(), Some("expired"));
        assert_eq!(fixture.stash.active(Provider::Codex).as_deref(), Some("other"));
    }

    #[test]
    fn a_codex_login_with_a_different_account_id_is_not_the_saved_active_account() {
        let fixture = Fixture::new("codex-identity");
        let g = codex_stashed("g", codex_oauth("r-g", LATER, "g@x"));
        fixture.stash.set_active(Provider::Codex, "g").unwrap();
        let unknown = codex_oauth("r-other", LATER + 1, "other@x");
        assert!(identify(&fixture.ctx(), &[g], Provider::Codex, Some(&unknown)).is_none());
    }

    #[test]
    fn users_sharing_a_codex_workspace_never_share_credentials() {
        let fixture = Fixture::new("codex-shared-workspace");
        let g = codex_stashed("g", codex_oauth("r-g", LATER, "g@x"));
        let mut other_oauth = codex_oauth("r-other", LATER, "other@x");
        other_oauth
            .extra
            .insert("accountId".into(), serde_json::Value::String(g.account.uuid.clone()));
        let other = codex_stashed("other", other_oauth);
        fixture.stash.save("g", &g.account).unwrap();
        fixture.stash.save("other", &other.account).unwrap();
        let g_path = pen_for(&fixture.ctx(), &g, &Live::default()).unwrap();
        let other_path = pen_for(&fixture.ctx(), &other, &Live::default()).unwrap();
        let accounts = vec![g.clone(), other.clone()];
        assert_eq!(
            identify(&fixture.ctx(), &accounts, Provider::Codex, Some(&other.account.oauth))
                .as_deref(),
            Some("other")
        );
        let mut other = other;
        let mut fresh = other.account.oauth.clone();
        fresh.refresh_token = "r-other-fresh".into();
        propagate(&fixture.ctx(), &mut other, &fresh, &Live::default()).unwrap();
        let g_store = codex::Store::at(&g_path, Some(&g_path));
        let other_store = codex::Store::at(&other_path, Some(&other_path));
        assert_eq!(g_store.read().unwrap().unwrap().oauth().unwrap().refresh_token, "r-g");
        assert_eq!(
            other_store.read().unwrap().unwrap().oauth().unwrap().refresh_token,
            "r-other-fresh"
        );
    }

    #[test]
    fn cached_status_reports_a_missing_reading_without_refreshing() {
        let fixture = Fixture::new("cached-unread");
        let g = codex_stashed("g", codex_oauth("expired", 1, "g@x"));
        fixture.stash.save("g", &g.account).unwrap();
        fixture.codex.write(&codex::AuthFile::default().with(&g.account.oauth)).unwrap();
        let entries = status_entries(&fixture.ctx(), true, Some(Provider::Codex)).unwrap();
        assert!(entries[0].entry.usage.as_ref().unwrap_err().contains("not polled"));
        assert!(entries[0].polled_at.is_none());
        assert_eq!(fixture.codex_live().as_deref(), Some("expired"));
    }

    #[test]
    fn a_cached_listing_marks_the_account_in_use_of_each_provider() {
        let fixture = Fixture::new("codex-cached");
        let a = stashed("a", oauth("r-a", LATER));
        let g = codex_stashed("g", codex_oauth("r-g", LATER, "g@x"));
        fixture.stash.save("a", &a.account).expect("save");
        fixture.stash.save("g", &g.account).expect("save");
        fixture.stash.set_active(Provider::Claude, "a").expect("active");
        fixture.stash.set_active(Provider::Codex, "g").expect("active");

        let entries = readings(&fixture.ctx()).expect("lists");
        assert!(entries.iter().all(|e| e.entry.active));
        assert_eq!(
            entries.iter().find(|e| e.entry.slug == "g").map(|e| e.entry.provider),
            Some(Provider::Codex)
        );
        assert_eq!(
            entries.iter().find(|e| e.entry.slug == "g").map(|e| e.entry.plan.as_str()),
            Some("codex ?")
        );
    }

    // ── the gateway's desk ──────────────────────────────────────────────────

    use crate::serve::Accounts as _;

    /// Far enough off that nothing here reaches for a refresh.
    const LATER: i64 = i64::MAX / 2;

    fn desk_fixture(name: &str, active: Option<&str>, pool: &[&str]) -> Fixture {
        let mut fixture = Fixture::new(name);
        for slug in ["a", "b", "c"] {
            let entry = stashed(slug, oauth(&format!("r-{slug}"), LATER));
            fixture.stash.save(&entry.slug, &entry.account).expect("stash");
        }
        if let Some(slug) = active {
            fixture.stash.set_active(Provider::Claude, slug).expect("active");
        }
        fixture.pool = pool.iter().map(|s| s.to_string()).collect();
        fixture
    }

    #[test]
    fn model_routes_apply_independently_and_never_change_the_active_account() {
        use crate::routing::{Routing, Rule};
        let fixture = desk_fixture("desk-model-routes", Some("c"), &["c"]);
        let routing = Routing {
            rules: vec![
                Rule {
                    provider: Provider::Claude,
                    model: "claude-opus-*".into(),
                    accounts: vec!["a".into()],
                },
                Rule {
                    provider: Provider::Claude,
                    model: "claude-fable-*".into(),
                    accounts: vec!["b".into(), "a".into()],
                },
            ],
        };
        routing.write(fixture.stash.root()).unwrap();
        let ctx = fixture.ctx();
        let desk = Desk { ctx: &ctx, pool: &fixture.pool };
        for _ in 0..4 {
            assert_eq!(
                desk.grant_model(Provider::Claude, Some("claude-opus-test"), None, &[])
                    .unwrap()
                    .slug,
                "a"
            );
            assert_eq!(
                desk.grant_model(Provider::Claude, Some("claude-fable-test"), None, &[])
                    .unwrap()
                    .slug,
                "b"
            );
        }
        assert_eq!(
            desk.grant_model(Provider::Claude, Some("claude-fable-test"), None, &["b".into()])
                .unwrap()
                .slug,
            "a"
        );
        assert!(
            desk.grant_model(Provider::Claude, Some("claude-opus-test"), None, &["a".into()])
                .is_err()
        );
        assert_eq!(
            desk.grant_model(Provider::Claude, Some("unmatched"), None, &[]).unwrap().slug,
            "c"
        );
        assert_eq!(fixture.stash.active(Provider::Claude).as_deref(), Some("c"));
        let mut changed = routing;
        changed.rules[0].accounts = vec!["b".into()];
        changed.write(fixture.stash.root()).unwrap();
        assert_eq!(
            desk.grant_model(Provider::Claude, Some("claude-opus-test"), None, &[]).unwrap().slug,
            "b"
        );
        changed.rules[0].accounts = vec!["missing".into()];
        changed.write(fixture.stash.root()).unwrap();
        assert!(desk.grant_model(Provider::Claude, Some("claude-opus-test"), None, &[]).is_err());
        std::fs::write(fixture.stash.root().join(crate::routing::FILE), "invalid").unwrap();
        assert!(desk.grant_model(Provider::Claude, Some("claude-opus-test"), None, &[]).is_err());
    }

    #[test]
    fn saving_one_route_preserves_other_provider_routes_and_active_logins() {
        use crate::routing::{Routing, Rule};
        let fixture = Fixture::new("save-route");
        for slug in ["a", "b"] {
            fixture.stash.save(slug, &stashed(slug, oauth(slug, LATER)).account).unwrap();
        }
        fixture
            .stash
            .save("g", &codex_stashed("g", codex_oauth("g", LATER, "g@x")).account)
            .unwrap();
        fixture.stash.set_active(Provider::Claude, "a").unwrap();
        fixture.stash.set_active(Provider::Codex, "g").unwrap();
        let ctx = fixture.ctx();
        let mut claude = Rule {
            provider: Provider::Claude,
            model: "future-model-v9".into(),
            accounts: vec!["b".into(), "a".into()],
        };
        save_route(&ctx, claude.clone()).unwrap();
        let codex = Rule {
            provider: Provider::Codex,
            model: claude.model.clone(),
            accounts: vec!["g".into()],
        };
        save_route(&ctx, codex.clone()).unwrap();
        claude.accounts.reverse();
        save_route(&ctx, claude.clone()).unwrap();
        assert_eq!(Routing::read(ctx.stash.root()).unwrap().rules, [claude, codex]);
        assert_eq!(ctx.stash.active(Provider::Claude).as_deref(), Some("a"));
        assert_eq!(ctx.stash.active(Provider::Codex).as_deref(), Some("g"));
        assert!(fixture.creds.read().unwrap().is_none());
        assert!(fixture.codex.read().unwrap().is_none());

        let before = fs::read(ctx.stash.root().join(crate::routing::FILE)).unwrap();
        for rule in [
            Rule { provider: Provider::Claude, model: "other".into(), accounts: vec!["g".into()] },
            Rule { provider: Provider::Claude, model: "other".into(), accounts: vec![] },
            Rule { provider: Provider::Claude, model: "*".into(), accounts: vec!["a".into()] },
        ] {
            assert!(save_route(&ctx, rule).is_err());
            assert_eq!(fs::read(ctx.stash.root().join(crate::routing::FILE)).unwrap(), before);
        }
    }

    #[test]
    fn a_grant_is_the_account_in_use() {
        let fixture = desk_fixture("desk-active", Some("b"), &[]);
        let ctx = fixture.ctx();
        let grant = Desk { ctx: &ctx, pool: &fixture.pool }
            .grant(Provider::Claude, None, &[])
            .expect("grants");
        assert_eq!((grant.slug.as_str(), grant.token.as_str()), ("b", "access-r-b"));
        assert_eq!(grant.email, "b@example.com");
    }

    #[test]
    fn a_limited_account_in_use_gives_way_to_the_pool_in_order() {
        let fixture = desk_fixture("desk-pool", Some("b"), &["c", "a"]);
        let ctx = fixture.ctx();
        let desk = Desk { ctx: &ctx, pool: &fixture.pool };
        assert_eq!(desk.grant(Provider::Claude, None, &["b".into()]).expect("grants").slug, "c");
        assert_eq!(
            desk.grant(Provider::Claude, None, &["b".into(), "c".into()]).expect("grants").slug,
            "a"
        );
    }

    #[test]
    fn with_every_account_limited_the_refusal_says_so() {
        let fixture = desk_fixture("desk-spent", Some("b"), &["a"]);
        let ctx = fixture.ctx();
        let why = Desk { ctx: &ctx, pool: &fixture.pool }
            .grant(Provider::Claude, None, &["b".into(), "a".into()])
            .expect_err("nothing left");
        assert!(why.contains("limited"), "{why}");
    }

    #[test]
    fn with_nothing_in_use_the_refusal_points_at_the_switch() {
        let fixture = desk_fixture("desk-none", None, &[]);
        let ctx = fixture.ctx();
        let why = Desk { ctx: &ctx, pool: &fixture.pool }
            .grant(Provider::Claude, None, &[])
            .expect_err("nothing in use");
        assert!(why.contains("ccs use"), "{why}");
    }

    #[test]
    fn with_nothing_in_use_the_pool_still_answers() {
        let fixture = desk_fixture("desk-pool-only", None, &["c"]);
        let ctx = fixture.ctx();
        assert_eq!(
            Desk { ctx: &ctx, pool: &fixture.pool }
                .grant(Provider::Claude, None, &[])
                .expect("grants")
                .slug,
            "c"
        );
    }

    #[test]
    fn a_pinned_grant_is_the_account_named_whatever_is_in_use_or_pooled() {
        let fixture = desk_fixture("desk-pinned", Some("b"), &["c"]);
        let ctx = fixture.ctx();
        let desk = Desk { ctx: &ctx, pool: &fixture.pool };
        let grant = desk.grant(Provider::Claude, Some("a"), &[]).expect("grants");
        assert_eq!((grant.slug.as_str(), grant.token.as_str()), ("a", "access-r-a"));
        // By email, and by prefix; the account in use is as good a pin as any.
        assert_eq!(desk.grant(Provider::Claude, Some("c@example.com"), &[]).unwrap().slug, "c");
        assert_eq!(desk.grant(Provider::Claude, Some("b@ex"), &[]).unwrap().slug, "b");
        assert_eq!(fixture.stash.active(Provider::Claude).as_deref(), Some("b"));
        assert_eq!(
            desk.grant(Provider::Claude, Some("nobody"), &[]).unwrap_err(),
            "no such account for claude: nobody"
        );
    }

    #[test]
    fn a_pin_outranks_a_model_route() {
        use crate::routing::{Routing, Rule};
        let fixture = desk_fixture("desk-pin-over-route", Some("c"), &[]);
        let routing = Routing {
            rules: vec![Rule {
                provider: Provider::Claude,
                model: "claude-opus-*".into(),
                accounts: vec!["a".into()],
            }],
        };
        routing.write(fixture.stash.root()).unwrap();
        let ctx = fixture.ctx();
        let desk = Desk { ctx: &ctx, pool: &fixture.pool };
        let routed = desk.grant_model(Provider::Claude, Some("claude-opus-test"), None, &[]);
        assert_eq!(routed.unwrap().slug, "a");
        let pinned = desk.grant_model(Provider::Claude, Some("claude-opus-test"), Some("b"), &[]);
        assert_eq!(pinned.unwrap().slug, "b");
    }

    /// One address signed up with both providers is two accounts; a pin on
    /// one provider's route names that provider's.
    #[test]
    fn a_pin_is_resolved_among_the_providers_own_accounts() {
        let mut shared = stashed("shared_claude", oauth("r-1", LATER));
        shared.account.email = "shared@example.com".into();
        let mut twin =
            codex_stashed("shared_codex", codex_oauth("r-2", LATER, "shared@example.com"));
        twin.account.email = "shared@example.com".into();
        let other = stashed("other", oauth("r-3", LATER));
        let accounts = vec![shared, twin, other];

        assert_eq!(
            pinned(&accounts, Provider::Claude, "shared@example.com").unwrap(),
            "shared_claude"
        );
        assert_eq!(
            pinned(&accounts, Provider::Codex, "shared@example.com").unwrap(),
            "shared_codex"
        );
        assert_eq!(pinned(&accounts, Provider::Claude, "SHARED_C").unwrap(), "shared_claude");
        assert_eq!(
            describe(&pinned(&accounts, Provider::Codex, "other").unwrap_err()),
            "no such account for codex: other"
        );
        // An ambiguous prefix is reported in `resolve`'s own words.
        let mut alike = stashed("shared_claude2", oauth("r-4", LATER));
        alike.account.email = "shared2@example.com".into();
        let accounts = [accounts, vec![alike]].concat();
        let why = describe(&pinned(&accounts, Provider::Claude, "shared").unwrap_err());
        assert!(why.contains("ambiguous"), "{why}");
    }

    /// A session refreshing the live credentials leaves the stash behind. A
    /// rejected token is the moment that shows: the copy a session holds is
    /// the one to hand out.
    #[test]
    fn a_rejected_token_is_replaced_by_the_newer_copy_a_session_holds() {
        let fixture = desk_fixture("desk-stale", Some("b"), &[]);
        fixture.creds.write(&CredsFile::new(oauth("r-b-2", LATER + 1))).expect("live");
        let ctx = fixture.ctx();
        let desk = Desk { ctx: &ctx, pool: &fixture.pool };
        let old = desk.grant(Provider::Claude, None, &[]).expect("grants");

        let renewed = desk.stale(&old).expect("answers").expect("a newer copy");
        assert_eq!(renewed.token, "access-r-b-2");
        // ...and the stash was brought up to date while it was at it.
        assert_eq!(fixture.tokens("b").0, "r-b-2");
    }

    /// A stash copy that has expired may already have been superseded by the
    /// session's own refresh. The newer copy is what gets handed out, and no
    /// refresh is attempted on the spent one.
    #[test]
    fn a_spent_stash_copy_is_brought_level_before_any_refresh_is_attempted() {
        let fixture = desk_fixture("desk-level", Some("b"), &[]);
        let spent = stashed("b", oauth("r-b", 0));
        fixture.stash.save(&spent.slug, &spent.account).expect("stash");
        fixture.creds.write(&CredsFile::new(oauth("r-b-2", LATER))).expect("live");
        let ctx = fixture.ctx();

        let grant = Desk { ctx: &ctx, pool: &fixture.pool }
            .grant(Provider::Claude, None, &[])
            .expect("grants");

        assert_eq!(grant.token, "access-r-b-2");
        assert_eq!(fixture.tokens("b").0, "r-b-2");
    }

    #[test]
    fn a_rejected_token_with_no_newer_copy_is_reported_as_such() {
        let fixture = desk_fixture("desk-fresh", Some("b"), &[]);
        let ctx = fixture.ctx();
        let desk = Desk { ctx: &ctx, pool: &fixture.pool };
        let grant = desk.grant(Provider::Claude, None, &[]).expect("grants");
        assert!(desk.stale(&grant).expect("answers").is_none());
    }
}
