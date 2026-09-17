//! Anthropic OAuth client: identity, usage, and token refresh.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;

use crate::model::{Oauth, Profile, UsageResponse, now_ms};

const API_BASE: &str = "https://api.anthropic.com";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";

/// Claude Code's own OAuth client. The tokens in the credentials file were
/// minted for it, so a refresh has to present the same id to be honoured.
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// The beta gating the OAuth surface.
const BETA: &str = "oauth-2025-04-20";

const TIMEOUT: Duration = Duration::from_secs(15);

/// Lifetime assumed when a refresh response declines to state one. Deliberately
/// short: guessing low costs an extra refresh, guessing high costs a failed
/// request at the worst moment.
const FALLBACK_LIFETIME_MS: i64 = 60 * 60 * 1000;

/// The way back from credentials the server will not accept. Logging in through
/// this tool mints the account afresh on its own, so it is never worth sending
/// anyone through a `claude` login, which would sign the account in use out.
const RELOGIN: &str = "`ccs add` logs this account in again without disturbing the one in use";

pub struct Api {
    agent: ureq::Agent,
}

impl Default for Api {
    fn default() -> Self {
        Self::new()
    }
}

/// The token endpoint's error shape, read to tell credentials that have been
/// superseded from every other reason a refresh can fail.
#[derive(Debug, Deserialize)]
struct TokenError {
    #[serde(default)]
    error: String,
}

/// Tokens as the refresh endpoint returns them.
#[derive(Debug, Clone, Deserialize)]
pub struct Refreshed {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Seconds.
    #[serde(default)]
    pub expires_in: Option<i64>,
    /// Codex's endpoint rotates the identity token too.
    #[serde(default)]
    pub id_token: Option<String>,
}

impl Api {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .user_agent(concat!("ccs/", env!("CARGO_PKG_VERSION")))
            // Read statuses rather than have them raised as transport errors:
            // a 401 here means a specific, actionable thing worth saying.
            .http_status_as_error(false)
            .build();
        Self { agent: ureq::Agent::new_with_config(config) }
    }

    /// Who a token belongs to.
    pub fn profile(&self, token: &str) -> Result<Profile> {
        self.get_json(&format!("{API_BASE}/api/oauth/profile"), token)
    }

    /// Current rate limit standing for a token.
    pub fn usage(&self, token: &str) -> Result<UsageResponse> {
        self.get_json(&format!("{API_BASE}/api/oauth/usage"), token)
    }

    /// Exact wire IDs from the provider catalog, following its pagination.
    pub fn models(&self, token: &str) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        let mut after = String::new();
        loop {
            let mut request = self
                .agent
                .get(format!("{API_BASE}/v1/models"))
                .header("Authorization", &format!("Bearer {token}"))
                .header("anthropic-version", "2023-06-01")
                .header("anthropic-beta", BETA)
                .query("limit", "1000");
            if !after.is_empty() {
                request = request.query("after_id", &after);
            }
            let mut response = request.call().context("loading Claude models")?;
            if response.status().as_u16() != 200 {
                bail!("Claude models returned {}", response.status().as_u16());
            }
            let page: ModelPage =
                response.body_mut().read_json().context("parsing Claude models")?;
            for model in page.data {
                if !model.id.is_empty() && !ids.contains(&model.id) {
                    ids.push(model.id);
                }
            }
            if !page.has_more {
                return Ok(ids);
            }
            let next = page.last_id.context("Claude models omitted the next page cursor")?;
            if next.is_empty() || next == after {
                bail!("Claude models returned a repeated page cursor");
            }
            after = next;
        }
    }

    /// Trade a refresh token for a fresh access token.
    ///
    /// The response may rotate the refresh token, and the caller must persist
    /// whatever comes back before doing anything else: losing a rotated refresh
    /// token costs an interactive re-login.
    pub fn refresh(&self, refresh_token: &str, scopes: &[String]) -> Result<Refreshed> {
        let body = json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": CLIENT_ID,
            "scope": scopes.join(" "),
        });
        let mut resp = self
            .agent
            .post(TOKEN_URL)
            .header("Content-Type", "application/json")
            .send_json(&body)
            .with_context(|| format!("POST {TOKEN_URL}"))?;

        let status = resp.status().as_u16();
        if status != 200 {
            let detail = resp.body_mut().read_to_string().unwrap_or_default();
            // A refresh token is spent by the refresh that presents it, so the
            // server rejecting one means a newer refresh has already replaced
            // it — or the whole account has been signed out. Either way the raw
            // rejection says nothing anyone can act on.
            if superseded(&detail) {
                bail!("these credentials no longer refresh; {RELOGIN}");
            }
            bail!("token refresh failed ({status}): {}", snippet(&detail));
        }
        resp.body_mut().read_json().context("parsing refresh response")
    }

    fn get_json<T: DeserializeOwned>(&self, url: &str, token: &str) -> Result<T> {
        let mut resp = self
            .agent
            .get(url)
            .header("Authorization", &format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .header("anthropic-beta", BETA)
            .call()
            .with_context(|| format!("GET {url}"))?;

        let status = resp.status().as_u16();
        if status == 429 {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|h| h.to_str().ok())
                .and_then(|h| h.parse::<u64>().ok())
                .unwrap_or(600);
            return Err(crate::usage::RateLimited { retry_after }.into());
        }
        if status == 401 {
            bail!("token rejected (401); {RELOGIN}");
        }
        if status != 200 {
            let detail = resp.body_mut().read_to_string().unwrap_or_default();
            bail!("{url} returned {status}: {}", snippet(&detail));
        }
        resp.body_mut().read_json().with_context(|| format!("parsing response from {url}"))
    }
}

#[derive(Deserialize)]
struct ModelPage {
    data: Vec<ModelId>,
    has_more: bool,
    last_id: Option<String>,
}

#[derive(Deserialize)]
struct ModelId {
    id: String,
}

/// Fold refreshed tokens into a credential blob, preserving every field the
/// refresh response does not speak to.
pub fn refreshed_oauth(prev: &Oauth, next: &Refreshed) -> Oauth {
    Oauth {
        access_token: next.access_token.clone(),
        refresh_token: next.refresh_token.clone().unwrap_or_else(|| prev.refresh_token.clone()),
        expires_at: now_ms() + next.expires_in.map_or(FALLBACK_LIFETIME_MS, |s| s * 1000),
        ..prev.clone()
    }
}

/// Whether a rejected refresh is the endpoint saying the token it was handed
/// has been spent or revoked, rather than anything a retry could get past.
fn superseded(body: &str) -> bool {
    serde_json::from_str::<TokenError>(body).is_ok_and(|e| e.error == "invalid_grant")
}

/// Error bodies are occasionally enormous; one line of it is enough to act on.
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
    use serde_json::Map;

    fn previous() -> Oauth {
        let mut extra = Map::new();
        extra.insert("rateLimitTier".into(), "default_claude_max_20x".into());
        Oauth {
            access_token: "old-access".into(),
            refresh_token: "old-refresh".into(),
            expires_at: 0,
            scopes: vec!["user:inference".into()],
            subscription_type: Some("max".into()),
            extra,
        }
    }

    #[test]
    fn usage_429_exposes_retry_after_without_retrying() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/usage", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut buf = [0; 4096];
            let read = socket.read(&mut buf).unwrap();
            assert!(read > 0);
            socket.write_all(b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1200\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        let error = Api::new().get_json::<UsageResponse>(&url, "fixture-token").unwrap_err();
        assert_eq!(error.downcast_ref::<crate::usage::RateLimited>().unwrap().retry_after, 1200);
        server.join().unwrap();
    }

    #[test]
    fn a_rotated_refresh_token_replaces_the_old_one() {
        let next = Refreshed {
            access_token: "new-access".into(),
            refresh_token: Some("new-refresh".into()),
            expires_in: Some(3600),
            id_token: None,
        };
        let folded = refreshed_oauth(&previous(), &next);
        assert_eq!(folded.access_token, "new-access");
        assert_eq!(folded.refresh_token, "new-refresh");
    }

    #[test]
    fn an_unrotated_refresh_token_is_kept_rather_than_blanked() {
        let next = Refreshed {
            access_token: "new-access".into(),
            refresh_token: None,
            expires_in: Some(60),
            id_token: None,
        };
        assert_eq!(refreshed_oauth(&previous(), &next).refresh_token, "old-refresh");
    }

    #[test]
    fn refreshing_preserves_every_field_the_response_is_silent_about() {
        let next = Refreshed {
            access_token: "new".into(),
            refresh_token: None,
            expires_in: Some(60),
            id_token: None,
        };
        let folded = refreshed_oauth(&previous(), &next);
        assert_eq!(folded.scopes, ["user:inference"]);
        assert_eq!(folded.subscription_type.as_deref(), Some("max"));
        assert_eq!(folded.rate_limit_tier(), Some("default_claude_max_20x"));
    }

    #[test]
    fn a_stated_lifetime_is_honoured() {
        let next = Refreshed {
            access_token: "new".into(),
            refresh_token: None,
            expires_in: Some(3600),
            id_token: None,
        };
        let slack = (refreshed_oauth(&previous(), &next).expires_at - now_ms() - 3_600_000).abs();
        assert!(slack < 5_000, "expiry should track the stated lifetime, off by {slack}ms");
    }

    #[test]
    fn a_spent_refresh_token_is_recognised_from_the_rejection() {
        let body = r#"{"error": "invalid_grant", "error_description": "Refresh token not found"}"#;
        assert!(superseded(body));
    }

    #[test]
    fn another_rejection_is_not_mistaken_for_a_spent_token() {
        assert!(!superseded(r#"{"error": "invalid_client"}"#));
    }

    #[test]
    fn a_rejection_that_is_not_the_endpoints_own_shape_is_left_to_speak_for_itself() {
        for body in ["", "<html>502 Bad Gateway</html>", "{}"] {
            assert!(!superseded(body), "{body:?} should not read as a spent token");
        }
    }

    #[test]
    fn a_missing_lifetime_falls_back_short_rather_than_optimistic() {
        let next = Refreshed {
            access_token: "new".into(),
            refresh_token: None,
            expires_in: None,
            id_token: None,
        };
        let folded = refreshed_oauth(&previous(), &next);
        assert!(folded.expires_at <= now_ms() + FALLBACK_LIFETIME_MS);
        assert!(folded.expires_at > now_ms());
    }
}
