//! The Codex side: the login Codex CLI keeps in `auth.json`, the OAuth
//! endpoints that refresh it, and the usage endpoint that says what a ChatGPT
//! subscription has left.
//!
//! A Codex account rides in the stash in the same shape as a Claude one —
//! access token, refresh token, expiry — with the identity token and the
//! ChatGPT account id alongside. Everything that keeps an account's copies in
//! step is shared; only what is read and written at the edges lives here.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::api::Refreshed;
use crate::fsx::write_atomic;
use crate::model::{
    Limit, LimitModel, LimitScope, ModelAvailability, Oauth, UsageResponse, now_ms,
};

/// Codex CLI's own OAuth client, which its refresh tokens were minted for.
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

/// The claim the account id and plan sit under in OpenAI's tokens.
const AUTH_CLAIM: &str = "https://api.openai.com/auth";

const TIMEOUT: Duration = Duration::from_secs(15);
const MODE: u32 = 0o600;

/// Lifetime assumed when a refresh response declines to state one.
const FALLBACK_LIFETIME_MS: i64 = 60 * 60 * 1000;

/// The lengths the two windows come in, as the CLI names them.
const SESSION_WINDOW: i64 = 5 * 3600;
const WEEKLY_WINDOW: i64 = 7 * 86400;

const RELOGIN: &str = "`ccs add` then choose Codex to log this account in again";

// ── auth.json ───────────────────────────────────────────────────────────────

/// `auth.json` as Codex CLI keeps it. Fields this tool has no opinion about
/// are carried through, so a switch never drops what a newer Codex wrote.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthFile {
    #[serde(default = "chatgpt")]
    pub auth_mode: String,
    #[serde(rename = "OPENAI_API_KEY", default)]
    pub openai_api_key: Option<String>,
    #[serde(default)]
    pub tokens: Option<Tokens>,
    #[serde(default)]
    pub last_refresh: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn chatgpt() -> String {
    "chatgpt".to_string()
}

impl Default for AuthFile {
    fn default() -> Self {
        Self {
            auth_mode: chatgpt(),
            openai_api_key: None,
            tokens: None,
            last_refresh: None,
            extra: Map::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tokens {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
    pub account_id: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl AuthFile {
    /// The ChatGPT login in this file, in the shape the stash keeps. A file
    /// logged in with an API key holds no account to stash. The expiry is
    /// read off the access token; one that cannot be read is taken for spent,
    /// which costs a refresh and nothing else.
    pub fn oauth(&self) -> Option<Oauth> {
        if self.auth_mode != "chatgpt" {
            return None;
        }
        let tokens = self.tokens.as_ref()?;
        let mut extra = Map::new();
        extra.insert("idToken".into(), Value::String(tokens.id_token.clone()));
        extra.insert("accountId".into(), Value::String(tokens.account_id.clone()));
        // A token whose expiry cannot be read is dated by the file's own
        // record of when it was minted: it then orders truthfully against
        // the other copies of the account, and reads as spent. Zero would
        // make it the oldest copy, and levelling would write over the
        // login Codex just rotated.
        let minted = self.last_refresh.as_deref().and_then(|s| s.parse::<Timestamp>().ok());
        Some(Oauth {
            access_token: tokens.access_token.clone(),
            refresh_token: tokens.refresh_token.clone(),
            expires_at: expiry(&tokens.access_token)
                .or_else(|| minted.map(|t| t.as_millisecond()))
                .unwrap_or(0),
            scopes: Vec::new(),
            subscription_type: None,
            extra,
        })
    }

    /// This file with `oauth` installed, everything else as it was.
    pub fn with(&self, oauth: &Oauth) -> Self {
        let mut tokens = self.tokens.clone().unwrap_or(Tokens {
            id_token: String::new(),
            access_token: String::new(),
            refresh_token: String::new(),
            account_id: String::new(),
            extra: Map::new(),
        });
        tokens.access_token = oauth.access_token.clone();
        tokens.refresh_token = oauth.refresh_token.clone();
        if let Some(id) = oauth.id_token() {
            tokens.id_token = id.to_string();
        }
        if let Some(account) = oauth.account_id() {
            tokens.account_id = account.to_string();
        }
        // Codex CLI reads this to decide when to refresh on its own, so it
        // has to say when these tokens were minted, not when they were put
        // here; the access token's own stamp is that, and what was there
        // stays when it cannot be read.
        let minted = issued(&oauth.access_token)
            .and_then(|s| Timestamp::from_second(s).ok())
            .map(|t| t.to_string())
            .or_else(|| self.last_refresh.clone());
        Self {
            auth_mode: chatgpt(),
            openai_api_key: self.openai_api_key.clone(),
            tokens: Some(tokens),
            last_refresh: minted,
            extra: self.extra.clone(),
        }
    }

    /// Whether this file holds an API-key login rather than a ChatGPT one.
    pub fn is_api_key(&self) -> bool {
        self.auth_mode != "chatgpt"
    }
}

// ── the store ───────────────────────────────────────────────────────────────

/// Where Codex's live login is kept. Shareable, like the Claude store.
pub trait Creds: Send + Sync {
    fn read(&self) -> Result<Option<AuthFile>>;
    fn write(&self, file: &AuthFile) -> Result<()>;
    /// The directory the lock is taken in.
    fn dir(&self) -> &Path;
    fn describe(&self) -> String;
}

pub struct Store {
    dir: PathBuf,
    path: PathBuf,
}

impl Store {
    /// The login under `codex_home` when Codex was told one, else `~/.codex`.
    pub fn at(home: &Path, codex_home: Option<&Path>) -> Self {
        let dir = codex_home.map(Path::to_path_buf).unwrap_or_else(|| home.join(".codex"));
        Self { path: dir.join("auth.json"), dir }
    }

    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Creds for Store {
    fn read(&self) -> Result<Option<AuthFile>> {
        let raw = match fs::read(&self.path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", self.path.display())),
        };
        serde_json::from_slice(&raw)
            .with_context(|| format!("parsing {}", self.path.display()))
            .map(Some)
    }

    fn write(&self, file: &AuthFile) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let body = serde_json::to_vec_pretty(file).context("serialising auth.json")?;
        write_atomic(&self.path, &body, MODE)
    }

    fn dir(&self) -> &Path {
        &self.dir
    }

    fn describe(&self) -> String {
        self.path.display().to_string()
    }
}

// ── tokens ──────────────────────────────────────────────────────────────────

/// Who a Codex account is, as its identity token says.
#[derive(Debug, Clone, PartialEq)]
pub struct Identity {
    pub email: String,
    pub plan: String,
    pub account_id: String,
}

pub fn identity(id_token: &str) -> Result<Identity> {
    let claims = claims(id_token)?;
    let auth = claims.get(AUTH_CLAIM).cloned().unwrap_or(Value::Null);
    let field = |value: &Value, name: &str| -> Result<String> {
        value
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("the identity token names no {name}"))
    };
    Ok(Identity {
        email: field(&claims, "email")?,
        plan: field(&auth, "chatgpt_plan_type").unwrap_or_else(|_| "?".into()),
        account_id: field(&auth, "chatgpt_account_id")?,
    })
}

/// When a JWT expires, in the milliseconds the stash keeps.
fn expiry(token: &str) -> Option<i64> {
    claims(token).ok()?.get("exp")?.as_i64().map(|s| s * 1000)
}

/// When a JWT was minted, in seconds.
fn issued(token: &str) -> Option<i64> {
    claims(token).ok()?.get("iat")?.as_i64()
}

/// A JWT's payload, unverified, for whoever needs to read a claim.
pub fn claims_of(token: &str) -> Result<Value> {
    claims(token)
}

/// A JWT's payload, unverified: the server that minted it is the one that
/// checks it, and what is read here is the account's own name for itself.
fn claims(token: &str) -> Result<Value> {
    let payload = token.split('.').nth(1).context("not a token: no payload")?;
    let bytes = base64url_decode(payload).context("the token's payload is not base64")?;
    serde_json::from_slice(&bytes).context("the token's payload is not JSON")
}

/// Base64url without padding, as JWTs are spelled.
pub fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n =
            chunk.iter().fold(0u32, |acc, b| (acc << 8) | u32::from(*b)) << (8 * (3 - chunk.len()));
        for i in 0..=chunk.len() {
            out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

fn base64url_decode(text: &str) -> Result<Vec<u8>> {
    let value = |c: u8| -> Result<u32> {
        Ok(match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            other => bail!("not base64: {:?}", other as char),
        })
    };
    let clean: Vec<u8> = text.bytes().filter(|b| *b != b'=').collect();
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    for chunk in clean.chunks(4) {
        if chunk.len() == 1 {
            bail!("not base64: a dangling character");
        }
        let mut n = 0u32;
        for c in chunk {
            n = (n << 6) | value(*c)?;
        }
        n <<= 6 * (4 - chunk.len());
        let bytes = n.to_be_bytes();
        out.extend_from_slice(&bytes[1..chunk.len()]);
    }
    Ok(out)
}

// ── usage ───────────────────────────────────────────────────────────────────

/// What `wham/usage` reports. Only the parts that become limits are read.
#[derive(Debug, Clone, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub rate_limit: Option<RateLimit>,
    #[serde(default)]
    pub additional_rate_limits: Option<Vec<NamedLimit>>,
    #[serde(default)]
    pub model_usage: Option<BTreeMap<String, ModelAvailability>>,
}

impl From<Usage> for UsageResponse {
    fn from(usage: Usage) -> Self {
        Self { limits: limits(&usage), model_usage: usage.model_usage }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RateLimit {
    #[serde(default)]
    pub primary_window: Option<Window>,
    #[serde(default)]
    pub secondary_window: Option<Window>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Window {
    pub used_percent: f64,
    pub limit_window_seconds: i64,
    #[serde(default)]
    pub reset_at: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NamedLimit {
    pub limit_name: String,
    #[serde(default)]
    pub rate_limit: Option<RateLimit>,
}

/// The usage as limits the rest of the tool knows: the two windows under the
/// names their lengths earn, and every window of every additional named pool.
pub fn limits(usage: &Usage) -> Vec<Limit> {
    let mut out = Vec::new();
    if let Some(rate) = &usage.rate_limit {
        for window in [&rate.primary_window, &rate.secondary_window].into_iter().flatten() {
            out.push(limit(kind_of(window), window, None));
        }
    }
    for named in usage.additional_rate_limits.iter().flatten() {
        let Some(rate) = &named.rate_limit else { continue };
        for window in [&rate.primary_window, &rate.secondary_window].into_iter().flatten() {
            out.push(limit(kind_of(window), window, Some(&named.limit_name)));
        }
    }
    out
}

fn kind_of(window: &Window) -> String {
    match window.limit_window_seconds {
        SESSION_WINDOW => "session".into(),
        WEEKLY_WINDOW => "weekly_all".into(),
        other => format!("{other}s"),
    }
}

fn limit(kind: impl Into<String>, window: &Window, model: Option<&str>) -> Limit {
    Limit {
        kind: kind.into(),
        percent: window.used_percent,
        severity: None,
        resets_at: window
            .reset_at
            .and_then(|s| Timestamp::from_second(s).ok())
            .map(|t| t.to_string()),
        scope: model.map(|name| LimitScope {
            model: Some(LimitModel { display_name: Some(name.to_string()) }),
        }),
    }
}

// ── the client ──────────────────────────────────────────────────────────────

pub struct Client {
    agent: ureq::Agent,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .user_agent(concat!("ccs/", env!("CARGO_PKG_VERSION")))
            .http_status_as_error(false)
            .build();
        Self { agent: ureq::Agent::new_with_config(config) }
    }

    /// Trade a refresh token for fresh tokens. The response rotates the
    /// refresh token; the caller persists what comes back.
    pub fn refresh(&self, refresh_token: &str) -> Result<Refreshed> {
        let body = json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": CLIENT_ID,
        });
        let mut resp = self
            .agent
            .post(TOKEN_URL)
            .header("Content-Type", "application/json")
            .send_json(&body)
            .with_context(|| format!("POST {TOKEN_URL}"))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        if status == 400 || status == 401 {
            bail!("these credentials no longer refresh; {RELOGIN}");
        }
        if status != 200 {
            bail!("token refresh failed ({status}): {}", snippet(&text));
        }
        serde_json::from_str(&text).context("parsing refresh response")
    }

    /// What the account has left.
    pub fn usage(&self, token: &str, account_id: &str) -> Result<Usage> {
        let mut resp = self
            .agent
            .get(USAGE_URL)
            .header("Authorization", &format!("Bearer {token}"))
            .header("chatgpt-account-id", account_id)
            .call()
            .with_context(|| format!("GET {USAGE_URL}"))?;
        let status = resp.status().as_u16();
        if status == 401 {
            bail!("token rejected (401); {RELOGIN}");
        }
        if status != 200 {
            let text = resp.body_mut().read_to_string().unwrap_or_default();
            bail!("{USAGE_URL} returned {status}: {}", snippet(&text));
        }
        resp.body_mut().read_json().context("parsing usage")
    }

    /// The same account-scoped catalog queried by Codex clients.
    pub fn models(
        &self,
        token: &str,
        account_id: &str,
        client_version: &str,
    ) -> Result<Vec<String>> {
        let mut response = self
            .agent
            .get("https://chatgpt.com/backend-api/codex/models")
            .query("client_version", client_version)
            .header("Authorization", &format!("Bearer {token}"))
            .header("chatgpt-account-id", account_id)
            .call()
            .context("loading Codex models")?;
        if response.status().as_u16() != 200 {
            bail!("Codex models returned {}", response.status().as_u16());
        }
        let catalog: Models = response.body_mut().read_json().context("parsing Codex models")?;
        Ok(catalog.visible_ids())
    }
}

#[derive(Deserialize)]
struct Models {
    models: Vec<ModelOption>,
}

#[derive(Deserialize)]
struct ModelOption {
    slug: String,
    visibility: String,
}

impl Models {
    fn visible_ids(self) -> Vec<String> {
        let mut ids = vec![];
        for model in self.models {
            if model.visibility == "list"
                && !model.slug.trim().is_empty()
                && !ids.contains(&model.slug)
            {
                ids.push(model.slug);
            }
        }
        ids
    }
}

#[test]
fn catalog_preserves_wire_ids_and_filters_hidden_models() {
    let catalog: Models = serde_json::from_str(
        r#"{"models":[
        {"slug":"new-model-v9","visibility":"list"},
        {"slug":"internal-model","visibility":"hide"},
        {"slug":"new-model-v9","visibility":"list"}
    ]}"#,
    )
    .unwrap();
    assert_eq!(catalog.visible_ids(), ["new-model-v9"]);
    assert!(serde_json::from_str::<Models>("{}").is_err());
}

/// Fold a refresh into the credentials: the new pair, the identity token
/// when the response carried one, and everything else as it was.
pub fn refreshed(prev: &Oauth, next: &Refreshed) -> Oauth {
    let mut extra = prev.extra.clone();
    if let Some(id) = &next.id_token {
        extra.insert("idToken".into(), Value::String(id.clone()));
    }
    let stated = next.expires_in.map(|s| now_ms() + s * 1000);
    Oauth {
        access_token: next.access_token.clone(),
        refresh_token: next.refresh_token.clone().unwrap_or_else(|| prev.refresh_token.clone()),
        expires_at: stated
            .or_else(|| expiry(&next.access_token))
            .unwrap_or_else(|| now_ms() + FALLBACK_LIFETIME_MS),
        scopes: prev.scopes.clone(),
        subscription_type: prev.subscription_type.clone(),
        extra,
    }
}

fn snippet(body: &str) -> String {
    let line = body.trim().lines().next().unwrap_or_default();
    match line.char_indices().nth(200) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Provider;

    /// A JWT whose payload is `claims`; the signature is not looked at.
    fn jwt(claims: serde_json::Value) -> String {
        let header = base64url(br#"{"alg":"RS256","typ":"JWT"}"#);
        let payload = base64url(claims.to_string().as_bytes());
        format!("{header}.{payload}.sig")
    }

    fn id_token(email: &str, plan: &str, account: &str) -> String {
        jwt(serde_json::json!({
            "email": email,
            "exp": 1_900_000_000,
            "https://api.openai.com/auth": {
                "chatgpt_account_id": account,
                "chatgpt_plan_type": plan,
            }
        }))
    }

    const AUTH_FILE: &str = r#"{
      "auth_mode": "chatgpt",
      "OPENAI_API_KEY": null,
      "tokens": {
        "id_token": "ID",
        "access_token": "ACCESS",
        "refresh_token": "REFRESH",
        "account_id": "acct-1"
      },
      "last_refresh": "2026-09-03T06:55:46.500555Z",
      "something_newer": {"x": 1}
    }"#;

    #[test]
    fn the_auth_file_is_read_into_the_shared_credential_shape() {
        let file: AuthFile = serde_json::from_str(AUTH_FILE).expect("parses");
        let oauth = file.oauth().expect("chatgpt tokens");
        assert_eq!(oauth.access_token, "ACCESS");
        assert_eq!(oauth.refresh_token, "REFRESH");
        assert_eq!(oauth.id_token(), Some("ID"));
        assert_eq!(oauth.account_id(), Some("acct-1"));
    }

    #[test]
    fn the_expiry_is_read_off_the_access_token_when_it_is_one() {
        let access = jwt(serde_json::json!({"exp": 1_900_000_000}));
        let raw = AUTH_FILE.replace("\"ACCESS\"", &format!("{access:?}"));
        let file: AuthFile = serde_json::from_str(&raw).expect("parses");
        assert_eq!(file.oauth().expect("tokens").expires_at, 1_900_000_000_000);
    }

    /// An access token whose expiry cannot be read is dated by the file's
    /// own record of when it was minted, so it orders truthfully against
    /// the other copies of the account, and reads as spent — one refresh.
    #[test]
    fn an_access_token_that_is_not_a_jwt_is_dated_by_the_files_last_refresh() {
        let file: AuthFile = serde_json::from_str(AUTH_FILE).expect("parses");
        let oauth = file.oauth().expect("tokens");
        let minted = "2026-09-03T06:55:46.500555Z".parse::<Timestamp>().expect("stamp");
        assert_eq!(oauth.expires_at, minted.as_millisecond());
        assert!(oauth.needs_refresh());
    }

    #[test]
    fn a_file_with_no_readable_expiry_at_all_is_taken_for_spent_long_ago() {
        let raw = AUTH_FILE.replace(r#""last_refresh": "2026-09-03T06:55:46.500555Z","#, "");
        let file: AuthFile = serde_json::from_str(&raw).expect("parses");
        assert_eq!(file.oauth().expect("tokens").expires_at, 0);
    }

    /// Codex CLI reads `last_refresh` to decide when to refresh on its own;
    /// a switch that stamped it now would tell it a stale token is fresh.
    /// The access token's own minting time is the truthful stamp.
    #[test]
    fn installing_credentials_stamps_last_refresh_with_the_tokens_own_minting_time() {
        let file: AuthFile = serde_json::from_str(AUTH_FILE).expect("parses");
        let mut oauth = file.oauth().expect("tokens");
        oauth.access_token = jwt(serde_json::json!({"exp": 1_900_000_000, "iat": 1_899_990_000}));
        let back = file.with(&oauth);
        assert_eq!(back.last_refresh.as_deref(), Some("2030-03-17T15:00:00Z"));

        // Nothing to read: the stamp that was there stays.
        oauth.access_token = "opaque".into();
        let back = file.with(&oauth);
        assert_eq!(back.last_refresh.as_deref(), Some("2026-09-03T06:55:46.500555Z"));
    }

    #[test]
    fn writing_credentials_back_keeps_every_field_the_file_had() {
        let file: AuthFile = serde_json::from_str(AUTH_FILE).expect("parses");
        let mut oauth = file.oauth().expect("tokens");
        oauth.access_token = "ACCESS2".into();
        let back = serde_json::to_value(file.with(&oauth)).expect("serialises");
        assert_eq!(back["tokens"]["access_token"], "ACCESS2");
        assert_eq!(back["tokens"]["refresh_token"], "REFRESH");
        assert_eq!(back["tokens"]["account_id"], "acct-1");
        assert_eq!(back["auth_mode"], "chatgpt");
        assert_eq!(back["something_newer"]["x"], 1);
        assert!(back["last_refresh"].as_str().is_some_and(|s| s.starts_with("20")));
    }

    #[test]
    fn a_file_logged_in_with_an_api_key_holds_no_account_to_stash() {
        let raw = AUTH_FILE.replace("\"chatgpt\"", "\"apikey\"");
        let file: AuthFile = serde_json::from_str(&raw).expect("parses");
        assert!(file.oauth().is_none());
    }

    #[test]
    fn a_fresh_file_is_made_around_credentials_when_there_is_none_to_keep() {
        let held: AuthFile = serde_json::from_str(AUTH_FILE).expect("parses");
        let fresh = AuthFile::default().with(&held.oauth().expect("tokens"));
        assert_eq!(fresh.oauth().expect("tokens").account_id(), Some("acct-1"));
        assert_eq!(fresh.auth_mode, "chatgpt");
    }

    #[test]
    fn who_an_account_is_comes_from_its_identity_token() {
        let who = identity(&id_token("you@x.com", "pro", "acct-9")).expect("claims");
        assert_eq!(who.email, "you@x.com");
        assert_eq!(who.plan, "pro");
        assert_eq!(who.account_id, "acct-9");
    }

    #[test]
    fn an_identity_token_without_the_claims_is_refused_with_a_reason() {
        let error = identity(&jwt(serde_json::json!({"sub": "x"}))).unwrap_err().to_string();
        assert!(error.contains("email"), "{error}");
        assert!(identity("not.a.jwt").is_err());
    }

    const USAGE: &str = r#"{
      "email": "you@x.com", "plan_type": "pro",
      "rate_limit": {
        "allowed": true, "limit_reached": false,
        "primary_window": {"used_percent": 16, "limit_window_seconds": 18000, "reset_after_seconds": 100, "reset_at": 1788750294},
        "secondary_window": {"used_percent": 29, "limit_window_seconds": 604800, "reset_after_seconds": 100, "reset_at": 1789305808}
      },
      "additional_rate_limits": [
        {"limit_name": "GPT-5.3-Codex-Spark", "metered_feature": "x",
         "rate_limit": {"allowed": true, "limit_reached": false,
           "primary_window": {"used_percent": 1, "limit_window_seconds": 18000, "reset_after_seconds": 1, "reset_at": 1788719008},
           "secondary_window": {"used_percent": 2, "limit_window_seconds": 604800, "reset_after_seconds": 1, "reset_at": 1789305808}}}
      ]
    }"#;

    #[test]
    fn the_two_windows_become_the_session_and_the_weekly_limit() {
        let usage: Usage = serde_json::from_str(USAGE).expect("parses");
        let limits = limits(&usage);
        assert_eq!(limits[0].kind, "session");
        assert_eq!(limits[0].percent, 16.0);
        assert_eq!(limits[0].resets_at.as_deref(), Some("2026-09-07T03:04:54Z"));
        assert_eq!(limits[1].kind, "weekly_all");
        assert_eq!(limits[1].percent, 29.0);
    }

    #[test]
    fn model_availability_survives_normalization_without_becoming_a_quota() {
        for available in [true, false] {
            let mut raw: Value = serde_json::from_str(USAGE).expect("fixture");
            let gate = json!({"gpt-6-astra": {
                "available": available, "available_at": null, "credits_would_enable": false
            }});
            raw["model_usage"] = gate.clone();
            let usage: Usage = serde_json::from_value(raw).expect("availability");
            let normalized = UsageResponse::from(usage);
            assert_eq!(normalized.limits.len(), 4);
            assert_eq!(normalized.limits[0].percent, 16.0);
            assert_eq!(serde_json::to_value(normalized.model_usage).unwrap(), gate);
        }
    }

    #[test]
    fn unreported_model_availability_stays_unknown() {
        for raw in [json!({}), json!({"model_usage": null})] {
            let usage: Usage = serde_json::from_value(raw).expect("optional metadata");
            assert!(UsageResponse::from(usage).model_usage.is_none());
        }
        let usage: Usage = serde_json::from_value(json!({
            "model_usage": {"future-model": {"credits_would_enable": true}}
        }))
        .expect("partial metadata");
        assert_eq!(usage.model_usage.unwrap()["future-model"].available, None);
    }

    #[test]
    fn a_named_extra_pool_keeps_both_windows_and_their_distinct_names() {
        let usage: Usage = serde_json::from_str(USAGE).expect("parses");
        let limits = limits(&usage);
        assert_eq!(limits.len(), 4);
        assert_eq!(limits[2].kind, "session");
        assert_eq!(limits[2].percent, 1.0);
        assert_eq!(limits[2].model_name(), Some("GPT-5.3-Codex-Spark"));
        assert_eq!(limits[3].kind, "weekly_all");
        assert_eq!(limits[3].percent, 2.0);
        assert_ne!(limits[2].column(), limits[3].column());
    }

    #[test]
    fn any_named_pool_keeps_every_reported_window_without_inventing_missing_ones() {
        let mut raw: Value = serde_json::from_str(USAGE).expect("fixture");
        raw["additional_rate_limits"].as_array_mut().unwrap().push(json!({
            "limit_name": "Another model pool",
            "rate_limit": {"primary_window": {
                "used_percent": 37, "limit_window_seconds": 3600, "reset_at": 1788719008_i64
            }}
        }));
        let usage: Usage = serde_json::from_value(raw).expect("parses");
        let limits = limits(&usage);
        assert_eq!(limits.len(), 5);
        let extra = &limits[4];
        assert_eq!(extra.model_name(), Some("Another model pool"));
        assert_eq!(extra.kind, "3600s");
        assert_eq!(extra.percent, 37.0);
        assert_eq!(extra.column(), "Another model pool 3600s");
    }

    #[test]
    fn null_or_absent_extra_pools_do_not_hide_the_shared_quota() {
        for extra in [json!(null), json!([])] {
            let mut raw: Value = serde_json::from_str(USAGE).expect("fixture");
            raw["additional_rate_limits"] = extra;
            let usage: Usage = serde_json::from_value(raw).expect("nullable pools");
            let limits = limits(&usage);
            assert_eq!(limits.len(), 2);
            assert!(limits.iter().all(|l| l.scope.is_none()));
        }
        let usage: Usage = serde_json::from_value(json!({})).expect("no windows");
        assert!(limits(&usage).is_empty());
    }

    #[test]
    fn an_incomplete_window_is_not_reported_as_zero_usage() {
        for window in [json!({"used_percent": 12}), json!({"limit_window_seconds": 18000})] {
            let raw = json!({"rate_limit": {"primary_window": window}});
            assert!(serde_json::from_value::<Usage>(raw).is_err());
        }
    }

    /// A Pro account reports a single weekly window and no five-hour one.
    #[test]
    fn a_lone_weekly_window_is_the_weekly_limit_not_the_session() {
        let raw = USAGE
            .replace(r#""primary_window": {"used_percent": 16, "limit_window_seconds": 18000"#, r#""primary_window": {"used_percent": 100, "limit_window_seconds": 604800"#)
            .replace(r#""secondary_window": {"used_percent": 29, "limit_window_seconds": 604800, "reset_after_seconds": 100, "reset_at": 1789305808}"#, r#""secondary_window": null"#);
        let usage: Usage = serde_json::from_str(&raw).expect("parses");
        let limits = limits(&usage);
        assert_eq!(limits[0].kind, "weekly_all");
        assert!(limits[0].exhausted());
        assert_eq!(limits.iter().filter(|l| l.scope.is_none()).count(), 1);
    }

    #[test]
    fn a_window_of_an_unfamiliar_length_is_named_by_its_length() {
        let raw = USAGE.replace(
            r#""limit_window_seconds": 18000, "reset_after_seconds": 100"#,
            r#""limit_window_seconds": 3600, "reset_after_seconds": 100"#,
        );
        let usage: Usage = serde_json::from_str(&raw).expect("parses");
        assert_eq!(limits(&usage)[0].kind, "3600s");
    }

    #[test]
    fn the_store_lives_where_codex_keeps_its_login() {
        let home = std::path::Path::new("/Users/you");
        assert_eq!(
            Store::at(home, None).path(),
            std::path::Path::new("/Users/you/.codex/auth.json")
        );
        assert_eq!(
            Store::at(home, Some(std::path::Path::new("/tmp/codex"))).path(),
            std::path::Path::new("/tmp/codex/auth.json")
        );
    }

    #[test]
    fn the_store_round_trips_credentials_and_says_when_there_are_none() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ccs-codex-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let store = Store::at(&dir, Some(&dir));

        assert!(store.read().expect("reads").is_none());
        let file: AuthFile = serde_json::from_str(AUTH_FILE).expect("parses");
        store.write(&file).expect("writes");
        let back = store.read().expect("reads").expect("a file");
        assert_eq!(back.oauth().expect("tokens").refresh_token, "REFRESH");
        let mode = std::fs::metadata(store.path()).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_codex_account_is_told_apart_by_its_provider() {
        assert_eq!(Provider::Codex.to_string(), "codex");
    }
}
