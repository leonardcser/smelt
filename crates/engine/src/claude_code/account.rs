//! Claude's account API, called with the login Claude Code stores and
//! refreshes: the plan usage report with the reset credits the account banks,
//! and redeeming them as Claude Code's own reset offer does.
//!
//! Claude Code's control protocol reports usage but not reset credits.

use serde_json::{json, Value};
use smelt_provider::usage::{
    parse_iso_time, UsageResetCredit, UsageResetCredits, UsageResetOption,
};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(super) const API_BASE: &str = "https://api.anthropic.com";
const PROGRAM: &str = "cedar_ember";

/// What account requests need from Claude Code's stored login.
pub(super) struct Login {
    token: String,
    organization: String,
    /// When the token expires, in Unix milliseconds.
    expires_at_ms: u64,
}

impl Login {
    /// Whether the token stays valid long enough for a request.
    pub fn lasts(&self) -> bool {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.expires_at_ms > now_ms + 5 * 60 * 1000
    }
}

/// Claude Code's configuration directory, which holds its login.
fn config_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| smelt_paths::home_dir().join(".claude"))
}

/// The file that names Claude Code's signed-in account.
fn account_config_path() -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir).join(".claude.json"),
        None => smelt_paths::home_dir().join(".claude.json"),
    }
}

/// The signed-in account, or nil when Claude Code is signed out.
fn account() -> Option<(String, String)> {
    let config: Value = serde_json::from_slice(&std::fs::read(account_config_path()).ok()?).ok()?;
    let account = &config["oauthAccount"];
    // The organization names a request path, so it must be a plain id.
    let organization = account["organizationUuid"].as_str()?.trim().to_owned();
    let user = account["accountUuid"].as_str().unwrap_or_default().trim();
    let plain = organization
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || c == b'-');
    (plain && !organization.is_empty()).then(|| (organization, user.to_owned()))
}

/// An opaque identity of the signed-in Claude account, so usage caches and
/// redemptions never cross accounts. It never contains credentials.
pub fn usage_scope() -> Option<String> {
    /// A revision of the account file: its path, modification time and size.
    type Revision = (PathBuf, SystemTime, u64);
    // The account file is rewritten often, so its scope is kept per revision.
    static LAST: Mutex<Option<(Revision, Option<String>)>> = Mutex::new(None);
    let path = account_config_path();
    let metadata = std::fs::metadata(&path).ok()?;
    let revision = (path, metadata.modified().ok()?, metadata.len());
    let mut last = LAST.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some((seen, scope)) = last.as_ref() {
        if *seen == revision {
            return scope.clone();
        }
    }
    let scope = account().map(|(organization, user)| {
        crate::auth::account_fingerprint(
            "claude-code",
            "account",
            &format!("{organization}\0{user}"),
        )
    });
    *last = Some((revision, scope.clone()));
    scope
}

/// Claude Code's stored login. macOS keeps it in the keychain, which smelt
/// does not read.
pub(super) fn login() -> Result<Login, String> {
    let signed_out = || "Sign in to Claude Code again to redeem resets.".to_string();
    if cfg!(target_os = "macos") {
        return Err("Redeeming Claude resets is not available on macOS yet.".into());
    }
    let credentials =
        std::fs::read(config_dir().join(".credentials.json")).map_err(|_| signed_out())?;
    let credentials: Value = serde_json::from_slice(&credentials)
        .map_err(|_| "Claude Code could not read its login.")?;
    let oauth = &credentials["claudeAiOauth"];
    let token = oauth["accessToken"]
        .as_str()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or_else(signed_out)?;
    let (organization, _) = account().ok_or_else(signed_out)?;
    Ok(Login {
        token: token.to_owned(),
        organization,
        expires_at_ms: oauth["expiresAt"].as_u64().unwrap_or_default(),
    })
}

fn headers(
    request: reqwest::RequestBuilder,
    token: &str,
    version: &str,
) -> reqwest::RequestBuilder {
    request
        .bearer_auth(token)
        .header("anthropic-beta", "oauth-2025-04-20")
        .header(
            "user-agent",
            format!("claude-cli/{version} (external, cli)"),
        )
}

/// The account's usage report with the reset credits it banks under
/// `cedar_ember`.
pub(super) async fn usage(
    client: &reqwest::Client,
    base: &str,
    login: &Login,
    version: &str,
) -> Result<Value, String> {
    let request = client.get(format!("{base}/api/oauth/usage?cedar_ember=1&skip_spend=1"));
    let response = headers(request, &login.token, version)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|_| "usage unavailable - check your connection")?;
    match response.status().as_u16() {
        200 => response
            .json()
            .await
            .map_err(|_| "usage response was invalid - try again later".into()),
        429 => Err("usage is rate limited - try again in a minute".into()),
        401 | 403 => Err("Claude Code login expired - run `claude` to sign in again".into()),
        _ => Err("usage unavailable right now - try again later".into()),
    }
}

/// How long reset credits are kept once read. Claude rate limits reading them
/// far more tightly than the plain usage report.
const CREDITS_TTL: Duration = Duration::from_secs(10 * 60);

/// The reset credits last read for one account.
struct KnownCredits {
    scope: String,
    read_at: Instant,
    credits: Option<UsageResetCredits>,
}

static KNOWN_CREDITS: Mutex<Option<KnownCredits>> = Mutex::new(None);

/// The reset credits the signed-in account banks, read at most once per
/// `CREDITS_TTL`. A failed read keeps the last known credits until then.
/// `model` names the model whose allowance a model reset refills.
pub(super) async fn reset_credits(
    client: &reqwest::Client,
    model: &str,
) -> Option<UsageResetCredits> {
    let scope = usage_scope()?;
    let known = KNOWN_CREDITS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .as_ref()
        .filter(|known| known.scope == scope)
        .map(|known| (known.read_at, known.credits.clone()));
    if let Some((read_at, credits)) = &known {
        if read_at.elapsed() < CREDITS_TTL {
            return credits.clone();
        }
    }
    let login = login().ok().filter(Login::lasts)?;
    let version = super::transport::version().await.ok()?;
    let credits = match usage(client, API_BASE, &login, &version).await {
        Ok(usage) => credits(&usage["cedar_ember"], model, now()),
        Err(_) => known.and_then(|(_, credits)| credits),
    };
    *KNOWN_CREDITS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = Some(KnownCredits {
        scope,
        read_at: Instant::now(),
        credits: credits.clone(),
    });
    credits
}

/// Drops the known reset credits, so the next report reads them again.
pub(super) fn forget_credits() {
    *KNOWN_CREDITS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = None;
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn valid_grant_id(id: &str) -> bool {
    (1..=40).contains(&id.len())
        && id
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_')
}

fn valid_request_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

struct Grant<'a> {
    id: &'a str,
    resets_left: u64,
    expires_at: Option<i64>,
    kind: Kind,
}

/// Claude Code's own grouping of grants into the resets it offers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Session,
    Weekly,
    Full,
    /// Refills the allowance of the model with its own weekly limit.
    Model,
}

impl Kind {
    /// Usage resets first, then the model's.
    const ALL: [Kind; 4] = [Kind::Session, Kind::Weekly, Kind::Full, Kind::Model];

    /// Names the reset after the limit rows usage shows.
    fn label(self, model: &str) -> String {
        match self {
            Kind::Session => "5h usage reset".into(),
            Kind::Weekly => "7d usage reset".into(),
            Kind::Full => "full usage reset".into(),
            Kind::Model => format!("{model} reset"),
        }
    }

    fn refills(self, model: &str) -> String {
        match self {
            Kind::Session => "5h limit".into(),
            Kind::Weekly => "7d limit".into(),
            Kind::Full => "5h + 7d limits".into(),
            Kind::Model => format!("{model} allowance"),
        }
    }

    /// What redeeming does, one short sentence per line.
    fn description(self, model: &str) -> String {
        match self {
            Kind::Session => "Refills your 5h limit.".into(),
            Kind::Weekly => "Refills your 7d limit.".into(),
            Kind::Full => "Refills your 5h and 7d limits.".into(),
            Kind::Model => format!(
                "Refills your {model} allowance.\nGives back what {model} used of your 7d limit.\nIt does not reset your whole week."
            ),
        }
    }

    /// A grant that gives back weekly usage is a model reset, else its
    /// declared type, else what it clears.
    fn of(grant: &Value) -> Kind {
        if grant["gives_back_weekly"] == true {
            return Kind::Model;
        }
        match grant["type"].as_str() {
            Some("session") => return Kind::Session,
            Some("weekly") => return Kind::Weekly,
            Some("full") => return Kind::Full,
            _ => {}
        }
        let clears = grant["clears"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let session = clears.iter().any(|limit| limit == "five_hour");
        let other = clears.iter().any(|limit| limit != "five_hour");
        match (session, other) {
            (true, false) => Kind::Session,
            (false, true) => Kind::Weekly,
            _ => Kind::Full,
        }
    }
}

fn grant(raw: &Value, now: i64) -> Option<Grant<'_>> {
    let id = raw["id"].as_str().filter(|id| valid_grant_id(id))?;
    let resets_left = raw["resets_left"].as_u64()?;
    if raw["paused"] == true || raw["usable_now"] != true {
        return None;
    }
    let expires_at = match &raw["ends_at"] {
        Value::Null => None,
        Value::String(at) => Some(parse_iso_time(at).filter(|at| *at > now)?),
        _ => return None,
    };
    Some(Grant {
        id,
        resets_left,
        expires_at,
        kind: Kind::of(raw),
    })
}

/// The reset credits a `cedar_ember` block offers, naming `model` for resets
/// of its allowance. `None` when the account cannot have any.
pub(super) fn credits(block: &Value, model: &str, now: i64) -> Option<UsageResetCredits> {
    if block["eligible"] != true {
        return None;
    }
    let live: Vec<Grant> = block["grants"]
        .as_array()?
        .iter()
        .filter_map(|raw| grant(raw, now))
        .collect();
    // Claude only redeems while it names the grant to spend next.
    if !live.iter().any(|grant| block["next_grant_id"] == grant.id) {
        return Some(UsageResetCredits {
            count: 0,
            inventory: None,
            options: Vec::new(),
        });
    }
    let inventory: Vec<UsageResetCredit> = live
        .iter()
        .flat_map(|grant| {
            (0..grant.resets_left).map(|_| UsageResetCredit {
                expires_at: grant.expires_at,
            })
        })
        .collect();
    let options = Kind::ALL
        .into_iter()
        .filter_map(|kind| {
            let mut grants = live
                .iter()
                .filter(|grant| grant.kind == kind && grant.resets_left > 0);
            let target = grants.next()?;
            Some(UsageResetOption {
                id: target.id.to_owned(),
                label: kind.label(model),
                refills: kind.refills(model),
                description: kind.description(model),
                count: target.resets_left + grants.map(|grant| grant.resets_left).sum::<u64>(),
                expires_at: target.expires_at,
            })
        })
        .collect();
    Some(UsageResetCredits {
        count: inventory.len() as u64,
        inventory: Some(inventory),
        options,
    })
}

/// Spends one reset of grant `grant_id`. `request_id` makes the claim
/// idempotent, so a retry after an uncertain outcome never spends twice.
pub(super) async fn claim(
    client: &reqwest::Client,
    base: &str,
    login: &Login,
    version: &str,
    grant_id: &str,
    request_id: &str,
) -> Result<String, String> {
    if !valid_grant_id(grant_id) || !valid_request_id(request_id) {
        return Err("Claude returned a malformed reset credit.".into());
    }
    let unconfirmed = || "Claude Code could not redeem the reset.".to_string();
    let request = client
        .post(format!(
            "{base}/api/organizations/{}/reset_rate_limits",
            login.organization
        ))
        .json(&json!({"program": PROGRAM, "grant_id": grant_id, "request_id": request_id}));
    let response = headers(request, &login.token, version)
        .timeout(Duration::from_secs(25))
        .send()
        .await
        .map_err(|_| unconfirmed())?;
    match response.status().as_u16() {
        200 => {}
        429 => return Err("Claude is rate limiting resets. Try again soon.".into()),
        401 | 403 => return Err("Sign in to Claude Code again to redeem resets.".into()),
        _ => return Err(unconfirmed()),
    }
    let body: Value = response.json().await.map_err(|_| unconfirmed())?;
    claim_message(&body)
}

fn claim_message(body: &Value) -> Result<String, String> {
    match body["result"].as_str() {
        Some("reset") => Ok("Redeemed reset credit.".into()),
        Some("not_limited") => Ok("No current usage limit window can be reset.".into()),
        Some("already_used") => Ok("This reset request was already redeemed.".into()),
        Some("ineligible") => Err("No usage limit reset credits are available.".into()),
        Some("cooldown") => Err("Claude resets are cooling down. Try again later.".into()),
        Some("unavailable") => Err(
            "Claude could not confirm the reset. If you are still limited in a moment, try again."
                .into(),
        ),
        _ => Err("Unexpected reset response.".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_893_456_000; // 2030-01-01

    fn grant(id: &str, resets_left: u64, extra: Value) -> Value {
        let mut grant = json!({"id": id, "resets_left": resets_left, "usable_now": true,
            "ends_at": "2030-01-31T00:00:00Z"});
        grant
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        grant
    }

    #[test]
    fn usage_resets_come_before_the_model_reset() {
        let offered = credits(
            &json!({"eligible": true, "next_grant_id": "weekly-a", "grants": [
                grant("weekly-a", 1, json!({"clears": ["seven_day"]})),
                grant("fable", 1, json!({"gives_back_weekly": true, "type": "weekly"})),
                grant("session", 2, json!({"type": "session", "ends_at": null})),
                grant("weekly-b", 1, json!({"type": "weekly"})),
                grant("both", 1, json!({"clears": ["five_hour", "seven_day"]})),
                grant("spent", 0, json!({"type": "full"})),
            ]}),
            "Fable",
            NOW,
        )
        .unwrap();
        let options: Vec<_> = offered
            .options
            .iter()
            .map(|option| {
                (
                    option.label.as_str(),
                    option.refills.as_str(),
                    option.id.as_str(),
                    option.count,
                    option.expires_at,
                )
            })
            .collect();
        let ends = parse_iso_time("2030-01-31T00:00:00Z");
        assert_eq!(
            options,
            [
                ("5h usage reset", "5h limit", "session", 2, None),
                ("7d usage reset", "7d limit", "weekly-a", 2, ends),
                ("full usage reset", "5h + 7d limits", "both", 1, ends),
                ("Fable reset", "Fable allowance", "fable", 1, ends),
            ]
        );
        assert_eq!(offered.count, 6);
        assert_eq!(offered.inventory.unwrap().len(), 6);
    }

    #[test]
    fn unusable_grants_are_never_offered() {
        let offered = credits(
            &json!({"eligible": true, "next_grant_id": "live", "grants": [
                grant("live", 1, json!({})),
                grant("paused", 1, json!({"paused": true})),
                grant("later", 1, json!({"usable_now": false})),
                grant("expired", 1, json!({"ends_at": "2029-12-31T00:00:00Z"})),
                grant("bad-date", 1, json!({"ends_at": "soon"})),
                grant("Bad Id", 1, json!({})),
            ]}),
            "Fable",
            NOW,
        )
        .unwrap();
        assert_eq!(offered.count, 1);
        assert_eq!(offered.options.len(), 1);
        assert_eq!(offered.options[0].id, "live");
    }

    #[test]
    fn credits_are_unavailable_without_the_next_grant() {
        let offered = credits(
            &json!({"eligible": true, "next_grant_id": null, "grants": [grant("a", 1, json!({}))]}),
            "Fable",
            NOW,
        )
        .unwrap();
        assert_eq!((offered.count, offered.options.len()), (0, 0));
        assert!(credits(&json!({"eligible": false, "grants": []}), "Fable", NOW).is_none());
        assert!(credits(&Value::Null, "Fable", NOW).is_none());
    }

    fn login() -> Login {
        Login {
            token: "fixture-token".into(),
            organization: "org-1".into(),
            expires_at_ms: u64::MAX,
        }
    }

    #[tokio::test]
    async fn claims_send_the_grant_and_idempotency_key() {
        let (host, request) =
            crate::provider::test_http::spawn_json_response(r#"{"result":"reset"}"#).await;
        let client = smelt_provider::http_client();
        let message = claim(&client, &host, &login(), "2.1.0", "grant-1", "smelt-1")
            .await
            .unwrap();
        assert_eq!(message, "Redeemed reset credit.");
        let request = request.await.unwrap();
        assert!(request.starts_with("POST /api/organizations/org-1/reset_rate_limits"));
        assert!(request.contains("fixture-token") && request.contains("claude-cli/2.1.0"));
        assert!(
            request.contains(r#""grant_id":"grant-1""#)
                && request.contains(r#""request_id":"smelt-1""#)
        );
        assert!(request.contains(r#""program":"cedar_ember""#));
    }

    #[tokio::test]
    async fn usage_is_read_with_claude_code_login() {
        let (host, request) =
            crate::provider::test_http::spawn_json_response(r#"{"five_hour":{}}"#).await;
        let client = smelt_provider::http_client();
        let report = usage(&client, &host, &login(), "2.1.0").await.unwrap();
        assert!(report["five_hour"].is_object());
        let request = request.await.unwrap();
        assert!(request.starts_with("GET /api/oauth/usage?cedar_ember=1&skip_spend=1 "));
        assert!(request.contains("fixture-token") && request.contains("oauth-2025-04-20"));
        assert!(request.contains("claude-cli/2.1.0"));

        let (host, _) =
            crate::provider::test_http::spawn_http_response("429 Too Many Requests", "{}").await;
        let error = usage(&client, &host, &login(), "2.1.0").await.unwrap_err();
        assert!(error.contains("rate limited"), "{error}");
    }

    #[tokio::test]
    async fn malformed_ids_are_never_sent() {
        let client = smelt_provider::http_client();
        let host = "http://127.0.0.1:9";
        for (grant, request) in [("Grant", "id"), ("grant", "bad id"), ("", "id")] {
            assert!(claim(&client, host, &login(), "2.1.0", grant, request)
                .await
                .unwrap_err()
                .contains("malformed"));
        }
    }

    #[tokio::test]
    async fn refused_claims_say_why() {
        let client = smelt_provider::http_client();
        for (status, expected) in [
            ("429 Too Many Requests", "rate limiting"),
            ("401 Unauthorized", "Sign in"),
        ] {
            let (host, _) = crate::provider::test_http::spawn_http_response(status, "{}").await;
            let error = claim(&client, &host, &login(), "2.1.0", "grant", "id")
                .await
                .unwrap_err();
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn claim_outcomes_are_explicit() {
        assert!(claim_message(&json!({"result": "not_limited"})).is_ok());
        assert!(claim_message(&json!({"result": "already_used"})).is_ok());
        for result in ["ineligible", "cooldown", "unavailable", "unknown"] {
            assert!(
                claim_message(&json!({"result": result})).is_err(),
                "{result}"
            );
        }
    }
}
