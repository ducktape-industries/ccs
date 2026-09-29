//! Domain types: the credential blob Claude Code stores on disk, the OAuth
//! API's responses, and the health verdicts derived from them.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// How long before genuine expiry an access token is treated as spent. A token
/// that expires mid-flight costs a retry, so buy it back cheaply up front.
const REFRESH_SKEW_MS: i64 = 60_000;

/// Percentage at or above which a limit has nothing left to give.
const EXHAUSTED_PCT: f64 = 100.0;

/// Percentage at or above which a limit is worth warning about.
const WARN_PCT: f64 = 80.0;

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

// ── providers ────────────────────────────────────────────────────────────────

/// Whose account this is. Each provider has a live slot of its own — Claude
/// Code's credentials, Codex's `auth.json` — and an account in use in it.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    #[default]
    Claude,
    Codex,
}

impl Provider {
    pub const ALL: [Provider; 2] = [Provider::Claude, Provider::Codex];

    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
        }
    }
}

/// One model as a provider's own catalog describes it: enough for a client
/// that has never heard of it to offer it, size its context and pick an effort.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogModel {
    /// The wire id, exactly as the API takes it.
    pub id: String,
    pub name: String,
    pub context_window: u64,
    pub max_tokens: u64,
    pub images: bool,
    /// Reasoning efforts the model takes, in the provider's own spelling;
    /// empty for a model that does not reason.
    pub efforts: Vec<String>,
    /// Thinking is adaptive only: effort, never a token budget.
    pub adaptive: bool,
    pub strict_tools: bool,
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        })
    }
}

impl FromStr for Provider {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            other => Err(format!("unknown provider {other:?}; claude or codex")),
        }
    }
}

// ── credentials on disk ──────────────────────────────────────────────────────

/// The `claudeAiOauth` object inside Claude Code's credentials file.
///
/// Fields this tool has no opinion about are captured in `extra` and written
/// back verbatim, so a round-trip through the stash never drops state belonging
/// to a newer Claude Code than the one this was built against.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Oauth {
    #[serde(rename = "accessToken")]
    pub access_token: String,
    #[serde(rename = "refreshToken")]
    pub refresh_token: String,
    /// Unix epoch milliseconds.
    #[serde(rename = "expiresAt")]
    pub expires_at: i64,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(rename = "subscriptionType", default, skip_serializing_if = "Option::is_none")]
    pub subscription_type: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Oauth {
    /// Whether the access token is spent, or close enough that refreshing now
    /// beats discovering it mid-request.
    pub fn needs_refresh(&self) -> bool {
        self.expires_at - now_ms() < REFRESH_SKEW_MS
    }

    /// Claude Code records the plan tier here on some versions and in the
    /// profile on others; prefer whichever is present.
    pub fn rate_limit_tier(&self) -> Option<&str> {
        self.extra.get("rateLimitTier").and_then(Value::as_str)
    }

    /// A Codex account's identity token, carried alongside the pair every
    /// provider has; it names the account and its plan.
    pub fn id_token(&self) -> Option<&str> {
        self.extra.get("idToken").and_then(Value::as_str)
    }

    /// A Codex account's ChatGPT account id, which every request names.
    pub fn account_id(&self) -> Option<&str> {
        self.extra.get("accountId").and_then(Value::as_str)
    }
}

/// Claude Code's credentials file as a whole.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredsFile {
    #[serde(rename = "claudeAiOauth")]
    pub oauth: Oauth,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl CredsFile {
    pub fn new(oauth: Oauth) -> Self {
        Self { oauth, extra: Map::new() }
    }
}

// ── stash entries ────────────────────────────────────────────────────────────

/// One stashed account: the identity used to label it, plus the credentials
/// needed to become it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    /// Absent in every stash file written before there was a choice.
    #[serde(default)]
    pub provider: Provider,
    pub email: String,
    pub uuid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_tier: Option<String>,
    pub added_at: String,
    pub oauth: Oauth,
}

impl Account {
    /// The plan, prefixed with the provider for every provider but the first,
    /// so a table of both reads without a column for it.
    pub fn plan_label(&self) -> String {
        let tier = self.rate_limit_tier.as_deref().or_else(|| self.oauth.rate_limit_tier());
        let plan = plan_label(tier, self.plan.as_deref());
        match self.provider {
            Provider::Claude => plan,
            other => format!("{other} {plan}"),
        }
    }
}

/// Short plan label for a column of them: the rate limit tier reads better than
/// the subscription type ("max20x" over "max"), so prefer it, and fall back to
/// whatever names the plan at all.
pub fn plan_label(tier: Option<&str>, plan: Option<&str>) -> String {
    match tier {
        Some(t) => t.trim_start_matches("default_claude_").replace('_', ""),
        None => plan.unwrap_or("?").to_string(),
    }
}

/// A stashed account together with the slug naming its file.
#[derive(Debug, Clone)]
pub struct Stashed {
    pub slug: String,
    pub account: Account,
}

// ── API responses ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub account: ProfileAccount,
    #[serde(default)]
    pub organization: Option<ProfileOrg>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileAccount {
    pub uuid: String,
    pub email: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileOrg {
    #[serde(default)]
    pub organization_type: Option<String>,
    #[serde(default)]
    pub rate_limit_tier: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageResponse {
    #[serde(default)]
    pub limits: Vec<Limit>,
    /// Model availability is independent of percentage-based quota windows.
    /// Absent in older caches and in responses that do not report it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_usage: Option<BTreeMap<String, ModelAvailability>>,
}

impl From<Vec<Limit>> for UsageResponse {
    fn from(limits: Vec<Limit>) -> Self {
        Self { limits, model_usage: None }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelAvailability {
    #[serde(default)]
    pub available: Option<bool>,
    #[serde(default)]
    pub available_at: Option<AvailableAt>,
    #[serde(default)]
    pub credits_would_enable: Option<bool>,
}

/// Keep the backend's timestamp representation in JSON and the cache. Either
/// an RFC 3339 instant or Unix seconds can provide a countdown; an unreadable
/// string never turns a blocked model into an available one.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AvailableAt {
    Timestamp(String),
    Seconds(i64),
}

impl AvailableAt {
    pub fn instant(&self) -> Option<jiff::Timestamp> {
        match self {
            Self::Timestamp(text) => text.parse().ok(),
            Self::Seconds(seconds) => jiff::Timestamp::from_second(*seconds).ok(),
        }
    }
}

/// One rate limit as the API reports it. The set is self-describing rather
/// than fixed, so new limit families and newly scoped models surface without
/// a code change here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Limit {
    pub kind: String,
    #[serde(default)]
    pub percent: f64,
    #[serde(default)]
    pub severity: Option<String>,
    #[serde(default)]
    pub resets_at: Option<String>,
    #[serde(default)]
    pub scope: Option<LimitScope>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitScope {
    #[serde(default)]
    pub model: Option<LimitModel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitModel {
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Ok,
    Warn,
    Critical,
}

impl Limit {
    /// The model this limit is scoped to, if it is scoped to one at all.
    pub fn model_name(&self) -> Option<&str> {
        self.scope.as_ref()?.model.as_ref()?.display_name.as_deref()
    }

    /// The column this limit belongs under.
    pub fn column(&self) -> String {
        if let Some(model) = self.model_name() {
            return match self.kind.as_str() {
                // Claude's scoped limits have always named their model alone.
                "weekly_scoped" => model.to_string(),
                _ => format!("{model} {}", self.window()),
            };
        }
        self.window()
    }

    /// The window independently of any model sharing it.
    pub fn window(&self) -> String {
        match self.kind.as_str() {
            "session" => "session".to_string(),
            "weekly_all" => "weekly".to_string(),
            other => other.replace('_', " "),
        }
    }

    /// Nothing left on this limit until it resets.
    pub fn exhausted(&self) -> bool {
        self.percent >= EXHAUSTED_PCT
    }

    /// The API's own severity is authoritative where it is reported; the
    /// percentage is the fallback for limit families that omit it.
    pub fn health(&self) -> Health {
        if self.exhausted() {
            return Health::Critical;
        }
        match self.severity.as_deref() {
            Some("critical") | Some("warning") => Health::Warn,
            Some("normal") => Health::Ok,
            _ if self.percent >= WARN_PCT => Health::Warn,
            _ => Health::Ok,
        }
    }
}

/// Build a `Limit` fixture. The field-by-field literal is unreadable repeated
/// a dozen times over, and every test wants a different two fields of it:
/// `limit!("session", 3.0)`, `limit!("weekly_scoped", 100.0, model = "Fable")`.
#[cfg(test)]
#[macro_export]
macro_rules! limit {
    ($kind:expr, $percent:expr $(, model = $model:expr)? $(, severity = $severity:expr)?
        $(, resets = $resets:expr)?) => {{
        #[allow(unused_mut)]
        let mut built = $crate::model::Limit {
            kind: $kind.to_string(),
            percent: $percent,
            severity: None,
            resets_at: None,
            scope: None,
        };
        $(
            built.scope = Some($crate::model::LimitScope {
                model: Some($crate::model::LimitModel {
                    display_name: Some($model.to_string()),
                }),
            });
        )?
        $( built.severity = Some($severity.to_string()); )?
        $( built.resets_at = Some($resets.to_string()); )?
        built
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oauth(expires_at: i64) -> Oauth {
        Oauth {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at,
            scopes: vec!["user:inference".into()],
            subscription_type: None,
            extra: Map::new(),
        }
    }

    #[test]
    fn column_names_the_model_for_scoped_limits() {
        assert_eq!(limit!("weekly_scoped", 100.0, model = "Fable").column(), "Fable");
    }

    #[test]
    fn column_folds_the_two_unscoped_families_to_short_names() {
        assert_eq!(limit!("session", 3.0).column(), "session");
        assert_eq!(limit!("weekly_all", 53.0).column(), "weekly");
    }

    #[test]
    fn column_falls_back_to_a_readable_form_of_an_unknown_kind() {
        assert_eq!(limit!("monthly_scoped", 1.0).column(), "monthly scoped");
    }

    #[test]
    fn exhausted_only_at_the_full_hundred() {
        assert!(!limit!("session", 99.9).exhausted());
        assert!(limit!("session", 100.0).exhausted());
        assert!(limit!("session", 140.0).exhausted());
    }

    #[test]
    fn health_treats_a_spent_limit_as_critical_whatever_the_severity() {
        assert_eq!(limit!("session", 100.0, severity = "normal").health(), Health::Critical);
    }

    #[test]
    fn health_defers_to_the_reported_severity_below_the_cap() {
        assert_eq!(limit!("weekly_all", 53.0, severity = "normal").health(), Health::Ok);
        assert_eq!(limit!("weekly_all", 12.0, severity = "critical").health(), Health::Warn);
    }

    #[test]
    fn health_falls_back_to_the_percentage_when_severity_is_absent() {
        assert_eq!(limit!("session", 85.0).health(), Health::Warn);
        assert_eq!(limit!("session", 20.0).health(), Health::Ok);
    }

    #[test]
    fn a_token_expiring_inside_the_skew_needs_refreshing() {
        assert!(oauth(now_ms() + 5_000).needs_refresh());
        assert!(oauth(now_ms() - 1).needs_refresh());
        assert!(!oauth(now_ms() + 10 * 60 * 1000).needs_refresh());
    }

    #[test]
    fn a_plan_label_prefers_the_tier_and_reads_it_short() {
        assert_eq!(plan_label(Some("default_claude_max_20x"), Some("max")), "max20x");
    }

    #[test]
    fn a_plan_label_falls_back_to_the_plan_then_to_a_question_mark() {
        assert_eq!(plan_label(None, Some("pro")), "pro");
        assert_eq!(plan_label(None, None), "?");
    }

    /// Every stash file written before there was a second provider names
    /// none, and is a Claude account.
    #[test]
    fn an_account_without_a_provider_is_a_claude_account() {
        let raw = r#"{"email":"a@x","uuid":"u","added_at":"t","oauth":{"accessToken":"a",
            "refreshToken":"r","expiresAt":1}}"#;
        let account: Account = serde_json::from_str(raw).expect("parses");
        assert_eq!(account.provider, Provider::Claude);
        let back = serde_json::to_value(&account).expect("serialises");
        assert_eq!(back["provider"], "claude");
    }

    #[test]
    fn a_codex_account_names_its_provider_and_keeps_its_tokens_in_the_same_shape() {
        let raw = r#"{"provider":"codex","email":"a@x","uuid":"acct","added_at":"t",
            "oauth":{"accessToken":"a","refreshToken":"r","expiresAt":1,
            "idToken":"id.tok.en","accountId":"acct"}}"#;
        let account: Account = serde_json::from_str(raw).expect("parses");
        assert_eq!(account.provider, Provider::Codex);
        assert_eq!(account.oauth.id_token(), Some("id.tok.en"));
        assert_eq!(account.oauth.account_id(), Some("acct"));
        assert_eq!(account.plan_label(), "codex ?");
    }

    #[test]
    fn a_provider_reads_and_prints_as_its_lowercase_name() {
        assert_eq!(Provider::Codex.to_string(), "codex");
        assert_eq!("codex".parse::<Provider>().expect("parses"), Provider::Codex);
        assert_eq!("claude".parse::<Provider>().expect("parses"), Provider::Claude);
        assert!("gemini".parse::<Provider>().is_err());
    }

    #[test]
    fn unknown_credential_fields_survive_a_round_trip() {
        let raw = r#"{"claudeAiOauth":{"accessToken":"a","refreshToken":"r","expiresAt":1,
            "scopes":["s"],"subscriptionType":"max","rateLimitTier":"default_claude_max_20x",
            "refreshTokenExpiresAt":9},"somethingNewer":{"x":1}}"#;
        let parsed: CredsFile = serde_json::from_str(raw).expect("parses");
        let back = serde_json::to_value(&parsed).expect("serialises");

        assert_eq!(back["claudeAiOauth"]["rateLimitTier"], "default_claude_max_20x");
        assert_eq!(back["claudeAiOauth"]["refreshTokenExpiresAt"], 9);
        assert_eq!(back["somethingNewer"]["x"], 1);
    }
}
