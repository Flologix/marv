//! The Marv plan: `marv login`, `marv logout`, `marv plan [--manage]` and `/plan` (marv-rs only, not in Marv).
//!
//! The paid plans (Founding; Pro + models, which comes with a capped OpenRouter key) are sold on flologixai.com. This
//! follows the contract the site and Marv OS share, so the three agree on the server's API and on the files:
//!
//! - **Free needs no account.** Nothing here calls the server unless the person runs one of these commands.
//! - **The server:** [`DEFAULT_URL`], or `MARV_PLAN_URL` (tests point it at a local stub). `POST /login/start` emails a
//!   6-digit code, `POST /login/verify` trades it for a bearer token, `GET /me` says what the plan is and gives its
//!   licence and model key, `POST /logout` revokes the token, `POST /portal` gives Stripe's billing page.
//! - **Storage:** `<config dir>/plan.json` (`~/.marv/plan.json`), 0600 and written atomically: email, token, plan,
//!   licence and a label for the key (its first 12 characters), never the key itself. The key goes where the person's
//!   OpenRouter key already lives, `apiKey` in config.json, so every existing path (the provider, `/setup`, `/config`)
//!   uses it. It's put there only when they have no OpenRouter key of their own: a key they set is never overwritten.
//!   Signing out removes `apiKey` only when it is still exactly the plan's key. plan.json keeps only a label, so the
//!   key is confirmed with `/me` first; when that can't be done the key is left where it is (a plan key that's no
//!   longer paid for just stops working, while removing a key the person set themselves would lose it).
//! - **The licence** is checked offline with the public key compiled in ([`MARV_LICENCE_PUBLIC_KEY`]). An invalid or
//!   expired one never locks or deletes anything: Marv just counts as Free.
//! - **The terminal:** every string from the server (errors, email, plan, status, model, dates, the portal's URL) goes
//!   through `printable()` before it's shown, so the server can't put escape sequences in front of the person.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use marv_core::printable;
use marv_provider::ProviderKind;
use marv_session::config::{ConfigStore, Env, FileConfig, default_config_dir};
use serde_json::{Value, json};

/// The plan server, unless `MARV_PLAN_URL` says otherwise.
pub const DEFAULT_URL: &str = "https://www.flologixai.com/api/marv-plan";

/// The licences' Ed25519 public key (32 raw bytes, base64). The development key pair for now: replaced before launch.
pub const MARV_LICENCE_PUBLIC_KEY: &str = "22NbM1++70RAtG1ISq8TidaCvysmNL94IQ5ugEHH1vo=";

/// The sign-in, beside config.json. Shared with Marv OS.
pub const PLAN_FILE: &str = "plan.json";

/// How long one request may take. The email round trip is the person's, not the server's.
const TIMEOUT: Duration = Duration::from_secs(20);

/// The server's answers are small JSON objects; anything far bigger isn't one.
const MAX_REPLY: usize = 256 * 1024;

/// The contract's limit on the device name the server records.
const MAX_DEVICE: usize = 80;

/// How much of a server's error sentence is shown.
const MAX_ERROR: usize = 300;

/// `MARV_PLAN_URL`, else [`DEFAULT_URL`]; without a trailing slash, so paths join cleanly.
pub fn base_url(env: &Env) -> String {
    let url = env.get("MARV_PLAN_URL").map(|u| u.trim()).filter(|u| !u.is_empty()).unwrap_or(DEFAULT_URL);
    url.trim_end_matches('/').to_string()
}

/// A plan's name as the person knows it.
pub fn plan_name(plan: &str) -> String {
    match plan {
        "founding" => "Founding".into(),
        "pro_models" => "Pro + models".into(),
        other => printable(other),
    }
}

/// What a valid licence says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Licence {
    pub plan: String,
    pub email: String,
    /// Seconds since the Unix epoch (the paid period's end plus 14 days' grace).
    pub expires: u64,
}

/// The compiled-in public key's bytes.
fn public_key() -> Vec<u8> {
    STANDARD.decode(MARV_LICENCE_PUBLIC_KEY).unwrap_or_default()
}

/// `<base64url(payload)>.<base64url(signature over the payload's base64url text)>`, checked with `public_key` and
/// unexpired at `now`. `None` for anything else: a bad signature, a payload that isn't version 1, an expired one.
/// Never an error: an invalid licence means Free, nothing worse.
pub fn verify_licence(text: &str, public_key: &[u8], now: u64) -> Option<Licence> {
    let (payload, signature) = text.trim().split_once('.')?;
    let signature = URL_SAFE_NO_PAD.decode(signature).ok()?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, public_key).verify(payload.as_bytes(), &signature).ok()?;
    let value: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
    if value.get("v")?.as_u64()? != 1 {
        return None;
    }
    let expires = value.get("expires")?.as_u64()?;
    if now >= expires {
        return None;
    }
    Some(Licence {
        plan: value.get("plan")?.as_str()?.to_string(),
        email: value.get("email").and_then(Value::as_str).unwrap_or_default().to_string(),
        expires,
    })
}

/// The plan this machine's licence gives, checked offline; `None` is Free (not signed in, or the licence doesn't hold).
pub fn licensed_plan(config_dir: &Path, now: u64) -> Option<Licence> {
    verify_licence(&PlanFile::load(config_dir)?.licence, &public_key(), now)
}

/// `~/.marv/plan.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanFile {
    pub email: String,
    pub token: String,
    pub plan: String,
    pub licence: String,
    /// `sk-or-v1-abc…`: which key the plan gave, without the key. `None` for a plan without models.
    pub key_label: Option<String>,
}

impl PlanFile {
    fn path(config_dir: &Path) -> PathBuf {
        config_dir.join(PLAN_FILE)
    }

    /// `None` when there's no sign-in, or the file can't be read as one (the person signs in again).
    pub fn load(config_dir: &Path) -> Option<Self> {
        let value: Value = serde_json::from_str(&std::fs::read_to_string(Self::path(config_dir)).ok()?).ok()?;
        let text = |k: &str| value.get(k).and_then(Value::as_str).map(str::to_string);
        Some(Self {
            email: text("email").unwrap_or_default(),
            token: text("token")?,
            plan: text("plan").unwrap_or_default(),
            licence: text("licence").unwrap_or_default(),
            key_label: text("keyLabel"),
        })
    }

    /// 0600 in a 0700 folder, via a temporary file renamed into place: it holds a bearer token.
    pub async fn save(&self, config_dir: &Path) -> std::io::Result<()> {
        let mut value = json!({ "email": self.email, "token": self.token, "plan": self.plan, "licence": self.licence });
        if let Some(label) = &self.key_label {
            value["keyLabel"] = Value::from(label.as_str());
        }
        let text = serde_json::to_string_pretty(&value).map_err(std::io::Error::other)? + "\n";
        marv_fs::private_file::write_private(&Self::path(config_dir), &text).await
    }

    fn remove(config_dir: &Path) -> std::io::Result<()> {
        match std::fs::remove_file(Self::path(config_dir)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

/// The contract's `keyLabel`: the key's first 12 characters and "…".
fn key_label(key: &str) -> String {
    format!("{}…", key.chars().take(12).collect::<String>())
}

/// A pro_models plan's allowance (`/me`'s `models`).
#[derive(Debug, Clone, PartialEq)]
pub struct Models {
    pub api_key: String,
    pub default_model: Option<String>,
    pub limit_usd: f64,
    pub used_usd: f64,
    pub resets_at: Option<String>,
}

/// `GET /me`.
#[derive(Debug, Clone, PartialEq)]
pub struct Me {
    pub email: String,
    pub plan: String,
    pub status: String,
    pub period_end: Option<u64>,
    /// What the subscription costs, ready to print ("$20/month"); None from a server without the field, or a price
    /// that isn't a plain amount, currency code and month/year.
    pub price: Option<String>,
    pub licence: String,
    pub models: Option<Models>,
}

/// `/me`'s `price` as a line can show it: "$20/month", "$9.50/month", "20 EUR/year". Only digits, a three-letter
/// code and our own words are printed, so nothing from the server reaches the terminal as is.
fn price_label(price: &Value) -> Option<String> {
    let amount = price.get("amount").and_then(Value::as_f64).filter(|a| a.is_finite() && *a >= 0.0)?;
    let currency =
        price.get("currency").and_then(Value::as_str).filter(|c| c.len() == 3 && c.bytes().all(|b| b.is_ascii_alphabetic()))?;
    let interval = price.get("interval").and_then(Value::as_str).filter(|i| matches!(*i, "month" | "year"))?;
    let number = if amount.fract() == 0.0 { format!("{amount:.0}") } else { format!("{amount:.2}") };
    Some(match currency.to_ascii_lowercase().as_str() {
        "usd" => format!("${number}/{interval}"),
        _ => format!("{number} {}/{interval}", currency.to_ascii_uppercase()),
    })
}

/// Something that can go into config.json as one value: no spaces, controls or anything outside visible ASCII.
fn plain_value(text: &str) -> bool {
    !text.is_empty() && text.len() <= 400 && text.bytes().all(|b| b.is_ascii_graphic())
}

impl Me {
    fn parse(value: &Value) -> Option<Self> {
        let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        // Only an OpenRouter key with a value that can be stored as one is used; anything else is no models.
        let models = value.get("models").filter(|m| m.is_object()).and_then(|m| {
            if m.get("provider").and_then(Value::as_str) != Some("openrouter") {
                return None;
            }
            Some(Models {
                api_key: text(m, "apiKey").filter(|k| plain_value(k))?,
                default_model: text(m, "defaultModel").filter(|k| plain_value(k)),
                limit_usd: m.get("limitUsd").and_then(Value::as_f64).unwrap_or(0.0),
                used_usd: m.get("usedUsd").and_then(Value::as_f64).unwrap_or(0.0),
                resets_at: text(m, "resetsAt"),
            })
        });
        Some(Self {
            email: text(value, "email").unwrap_or_default(),
            plan: text(value, "plan")?,
            status: text(value, "status").unwrap_or_default(),
            period_end: value.get("periodEnd").and_then(Value::as_u64),
            price: value.get("price").and_then(price_label),
            licence: text(value, "licence").unwrap_or_default(),
            models,
        })
    }
}

/// Why a request didn't give an answer.
#[derive(Debug, Clone, PartialEq)]
pub enum CallError {
    /// No connection, a timeout, a TLS failure.
    Unreachable(String),
    /// The server said no: its status and its `error` sentence (already printable), or a stand-in.
    Refused { status: u16, error: String },
    /// A success that isn't JSON (a captive portal's page, say).
    BadReply,
}

impl CallError {
    fn text(&self) -> String {
        match self {
            CallError::Unreachable(e) => format!("couldn't reach the Marv plan server ({})", printable(e)),
            CallError::Refused { error, .. } => error.clone(),
            CallError::BadReply => "the Marv plan server's answer didn't make sense".into(),
        }
    }
}

/// Cuts `text` to `max` characters, with "…" when something was cut.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max { text.to_string() } else { text.chars().take(max).collect::<String>() + "…" }
}

/// One request to the plan server: JSON in and out, the token as a bearer credential. Redirects aren't followed,
/// so the token is only ever sent to the URL it was meant for.
async fn call(base: &str, post: bool, path: &str, token: Option<&str>, body: Option<&Value>) -> Result<Value, CallError> {
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .user_agent(format!("marv/{}", crate::update::CURRENT))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| CallError::Unreachable(e.to_string()))?;
    let url = format!("{base}{path}");
    let mut request = if post { client.post(url) } else { client.get(url) }.header("Accept", "application/json");
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    if post {
        let body = body.cloned().unwrap_or_else(|| json!({}));
        request = request.header("Content-Type", "application/json").body(body.to_string());
    }
    let response = request.send().await.map_err(|e| CallError::Unreachable(e.to_string()))?;
    let status = response.status();
    let bytes = response.bytes().await.map_err(|e| CallError::Unreachable(e.to_string()))?;
    let value: Option<Value> = (bytes.len() <= MAX_REPLY).then(|| serde_json::from_slice(&bytes).ok()).flatten();
    if !status.is_success() {
        // Only the `error` sentence, never the body itself: a proxy's HTML page says nothing the person can use.
        let error = value
            .as_ref()
            .and_then(|v| v.get("error"))
            .and_then(Value::as_str)
            .map(|e| clip(printable(e).trim(), MAX_ERROR))
            .filter(|e| !e.is_empty())
            .unwrap_or_else(|| format!("the Marv plan server answered HTTP {}", status.as_u16()));
        return Err(CallError::Refused { status: status.as_u16(), error });
    }
    value.ok_or(CallError::BadReply)
}

async fn fetch_me(base: &str, token: &str) -> Result<Me, CallError> {
    Me::parse(&call(base, false, "/me", Some(token), None).await?).ok_or(CallError::BadReply)
}

/// Where and when: the server, the config folder, the environment (for `OPENROUTER_API_KEY`), the clock.
pub struct Ctx {
    pub base: String,
    pub config_dir: PathBuf,
    pub env: Env,
    pub now: u64,
}

impl Ctx {
    /// This process's: the real environment, `~/.marv` or `MARV_CONFIG_DIR`.
    pub fn from_process() -> Self {
        let env: Env = std::env::vars().collect();
        let config_dir = default_config_dir(&env, &marv_session::paths::home_dir());
        Self { base: base_url(&env), config_dir, env, now: crate::update::now() }
    }

    /// For the TUI, which already has both.
    pub fn new(env: &Env, config_dir: &Path) -> Self {
        Self { base: base_url(env), config_dir: config_dir.to_path_buf(), env: env.clone(), now: crate::update::now() }
    }
}

/// `2026-12-01` for a Unix time (UTC). Howard Hinnant's days-to-civil, so no date crate is needed for one line.
fn date(unix: u64) -> String {
    let z = (unix / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// The date part of `resetsAt` (`2026-12-01T00:00:00Z` → `2026-12-01`); anything else, printable and short.
fn reset_date(text: &str) -> String {
    let day = text.get(..10).filter(|d| {
        let b = d.as_bytes();
        b.iter().enumerate().all(|(i, c)| if i == 4 || i == 7 { *c == b'-' } else { c.is_ascii_digit() })
    });
    day.map_or_else(|| clip(&printable(text), 40), str::to_string)
}

fn allowance_line(models: &Models) -> String {
    let left = (models.limit_usd - models.used_usd).max(0.0);
    let resets = models.resets_at.as_deref().map(|r| format!("; it resets on {}", reset_date(r))).unwrap_or_default();
    format!(
        "Model allowance: ${:.2} of ${:.2} used this month, ${left:.2} left{resets}.",
        models.used_usd.max(0.0),
        models.limit_usd
    )
}

/// `marv cli on <hostname>`, at most [`MAX_DEVICE`] characters of visible ASCII.
fn device_name(hostname: &str) -> String {
    let host: String = hostname.chars().filter(|c| c.is_ascii_graphic()).collect();
    let host = if host.is_empty() { "this machine".to_string() } else { host };
    format!("marv cli on {host}").chars().take(MAX_DEVICE).collect()
}

fn this_hostname() -> String {
    nix::unistd::gethostname().map(|h| h.to_string_lossy().into_owned()).unwrap_or_default()
}

/// The token is 64 hex digits; anything else isn't one to store.
fn is_token(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

fn plausible_email(text: &str) -> bool {
    text.len() <= 254 && !text.chars().any(|c| c.is_whitespace() || c.is_control()) && {
        let (user, domain) = text.rsplit_once('@').unwrap_or_default();
        !user.is_empty() && domain.contains('.')
    }
}

/// What signing in did to config.json.
#[derive(Debug, Clone, PartialEq, Eq)]
enum KeyChange {
    /// The plan's key is the OpenRouter key now; `model` set when Marv picked the plan's default.
    Set { model: Option<String> },
    /// The person's own OpenRouter key is there, and stays.
    KeptOwn,
}

/// Puts the plan's key in config.json, unless the person has an OpenRouter key of their own there. The contract's
/// rule: provider openrouter and `apiKey`, and `model` = the plan's default only when they had no OpenRouter model of
/// their own. A `baseUrl` saved for another provider (Ollama on another machine) would send the key there: it goes.
fn use_plan_key(config_dir: &Path, models: &Models) -> Result<KeyChange, String> {
    let store = ConfigStore::new(config_dir);
    let mut file = store.load().map_err(|e| e.0)?.unwrap_or_default();
    if file.api_key.as_deref().is_some_and(|key| key != models.api_key) {
        return Ok(KeyChange::KeptOwn);
    }
    let was_openrouter = file.provider == Some(ProviderKind::OpenRouter);
    let keep_model = was_openrouter && file.model.is_some();
    let model = (!keep_model).then(|| models.default_model.clone()).flatten();
    let next = FileConfig {
        provider: Some(ProviderKind::OpenRouter),
        custom_provider: None,
        api_key: Some(models.api_key.clone()),
        model: if keep_model { file.model.take() } else { model.clone() },
        base_url: if was_openrouter { file.base_url.take() } else { None },
        ..file
    };
    store.save(&next).map_err(|e| format!("Couldn't save {}: {e}", store.path.display()))?;
    Ok(KeyChange::Set { model })
}

/// Takes `key` out of config.json if it's still the OpenRouter key there. Whether it did.
fn drop_plan_key(config_dir: &Path, key: &str) -> Result<bool, String> {
    let store = ConfigStore::new(config_dir);
    let Some(mut file) = store.load().map_err(|e| e.0)? else { return Ok(false) };
    if file.api_key.as_deref() != Some(key) {
        return Ok(false);
    }
    file.api_key = None;
    store.save(&file).map_err(|e| format!("Couldn't save {}: {e}", store.path.display()))?;
    Ok(true)
}

/// Writes a line; a closed stdout isn't worth failing over.
macro_rules! say {
    ($out:expr, $($arg:tt)*) => {{
        let _ = writeln!($out, $($arg)*);
    }};
}

/// `marv login`: the email (asked when not given), the code from it, then the plan saved and, for a plan with
/// models, its key put to use. `ask` reads one answer (`None`: no more input).
pub async fn login(
    ctx: &Ctx,
    email: Option<String>,
    device: &str,
    ask: &mut dyn FnMut(&str) -> Option<String>,
    out: &mut dyn Write,
) -> Result<(), String> {
    let email = match email {
        Some(email) => email,
        None => ask("Email: ").ok_or("No email given.")?,
    };
    let email = email.trim().to_string();
    if !plausible_email(&email) {
        return Err(format!("\"{}\" doesn't look like an email address.", printable(&email)));
    }
    call(&ctx.base, true, "/login/start", None, Some(&json!({ "email": email })))
        .await
        .map_err(|e| format!("Couldn't start the sign-in: {}.", e.text()))?;
    say!(out, "If {email} has a Marv plan, a 6-digit code is on its way there (valid for 10 minutes).");
    let mut token = None;
    for _ in 0..3 {
        let Some(code) = ask("Code: ") else { break };
        let code: String = code.chars().filter(|c| !c.is_whitespace() && *c != '-').collect();
        if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
            say!(out, "The code is the 6 digits in the email.");
            continue;
        }
        let body = json!({ "email": email, "code": code, "device": device });
        match call(&ctx.base, true, "/login/verify", None, Some(&body)).await {
            Ok(reply) => {
                let found = reply.get("token").and_then(Value::as_str).filter(|t| is_token(t));
                token = Some(found.ok_or_else(|| format!("Couldn't sign in: {}.", CallError::BadReply.text()))?.to_string());
                break;
            }
            // A wrong or expired code: the server's sentence says which, and the person tries again.
            Err(CallError::Refused { status: 400, error }) => say!(out, "{error}"),
            Err(e) => return Err(format!("Couldn't sign in: {}.", e.text())),
        }
    }
    let token = token.ok_or("Not signed in. Run `marv login` again for a new code.")?;
    let me = fetch_me(&ctx.base, &token).await.map_err(|e| format!("Signed in, but couldn't read the plan: {}.", e.text()))?;
    let key_label = me.models.as_ref().map(|m| key_label(&m.api_key));
    let file = PlanFile { email: me.email.clone(), token, plan: me.plan.clone(), licence: me.licence.clone(), key_label };
    file.save(&ctx.config_dir).await.map_err(|e| format!("Couldn't save {}: {e}", PlanFile::path(&ctx.config_dir).display()))?;

    say!(out, "Signed in as {}: Marv {} ({}).", printable(&me.email), plan_name(&me.plan), printable(&me.status));
    if verify_licence(&me.licence, &public_key(), ctx.now).is_none() {
        say!(out, "The plan's licence didn't check out on this machine, so Marv counts as Free here for now.");
    }
    match &me.models {
        Some(models) => {
            match use_plan_key(&ctx.config_dir, models) {
                Ok(KeyChange::Set { model }) => {
                    let model = model.map(|m| format!(", model {}", printable(&m))).unwrap_or_default();
                    say!(out, "Marv now uses OpenRouter with your plan's key{model}.");
                    if ctx.env.get("OPENROUTER_API_KEY").is_some_and(|k| !k.trim().is_empty()) {
                        say!(
                            out,
                            "OPENROUTER_API_KEY is set in your environment, and it wins over the plan's key: unset it to use the plan."
                        );
                    }
                }
                Ok(KeyChange::KeptOwn) => say!(
                    out,
                    "You have your own OpenRouter key in config.json, so Marv keeps using it. To use the plan's allowance \
                     instead, delete \"apiKey\" from {} and run `marv login` again.",
                    ConfigStore::new(&ctx.config_dir).path.display()
                ),
                Err(e) => say!(out, "Couldn't put the plan's key in your config: {e}"),
            }
            say!(out, "{}", allowance_line(models));
        }
        None if me.plan == "pro_models" => {
            say!(out, "The plan's model allowance is off while the plan is {}.", printable(&me.status));
        }
        None => say!(out, "This plan has no model allowance: Marv keeps using your own provider and key."),
    }
    Ok(())
}

/// `marv logout`: the token revoked (a network failure doesn't stop the rest), the plan's key out of config.json if
/// it's still there and confirmed as the plan's, plan.json deleted.
pub async fn logout(ctx: &Ctx, out: &mut dyn Write) -> Result<(), String> {
    let Some(file) = PlanFile::load(&ctx.config_dir) else {
        say!(out, "You're not signed in to a Marv plan.");
        return Ok(());
    };
    // /me before /logout: afterwards the token can't ask any more.
    let plan_key = match fetch_me(&ctx.base, &file.token).await {
        Ok(me) => me.models.map(|m| m.api_key),
        Err(_) => None,
    };
    if let Err(e) = call(&ctx.base, true, "/logout", Some(&file.token), None).await {
        say!(out, "Couldn't tell the server ({}); signing out on this machine anyway.", e.text());
    }
    match plan_key.map(|key| drop_plan_key(&ctx.config_dir, &key)) {
        Some(Ok(true)) => say!(
            out,
            "Removed the plan's OpenRouter key from config.json. Run /setup in marv (or start it) to choose a provider."
        ),
        Some(Err(e)) => say!(out, "Couldn't remove the plan's key from your config: {e}"),
        Some(Ok(false)) => {}
        None if file.key_label.is_some() => say!(
            out,
            "Couldn't confirm which key is the plan's, so config.json's \"apiKey\" is left as it is. If it's the plan's \
             ({}), it stops working when the plan ends; /setup replaces it.",
            printable(file.key_label.as_deref().unwrap_or_default())
        ),
        None => {}
    }
    PlanFile::remove(&ctx.config_dir)
        .map_err(|e| format!("Couldn't delete {}: {e}", PlanFile::path(&ctx.config_dir).display()))?;
    say!(out, "Signed out of the Marv plan. Marv Free needs no account.");
    Ok(())
}

/// What `marv plan` and `/plan` show, and whether it's an error.
pub async fn status(ctx: &Ctx) -> (String, bool) {
    let Some(file) = PlanFile::load(&ctx.config_dir) else {
        return ("You're on Marv Free: no account needed. `marv login` signs in to a paid Marv plan.".into(), false);
    };
    let me = match fetch_me(&ctx.base, &file.token).await {
        Ok(me) => me,
        Err(CallError::Refused { status: 401, .. }) => {
            return ("Your Marv plan sign-in has expired or was revoked: run `marv login` again.".into(), true);
        }
        Err(e) => {
            let offline = match licensed_plan(&ctx.config_dir, ctx.now) {
                Some(l) => format!("This machine's licence: Marv {}, valid until {}.", plan_name(&l.plan), date(l.expires)),
                None => "This machine's licence isn't valid, so Marv counts as Free.".into(),
            };
            return (format!("Couldn't check your Marv plan: {}.\n{offline}", e.text()), true);
        }
    };
    // The licence is renewed with each period: keep the newest one for the offline check.
    if me.licence != file.licence || me.plan != file.plan {
        let _ = PlanFile { plan: me.plan.clone(), licence: me.licence.clone(), ..file.clone() }.save(&ctx.config_dir).await;
    }
    let price = me.price.as_deref().map(|p| format!(" at {p}")).unwrap_or_default();
    let mut lines =
        vec![format!("Marv {}{price} ({}), signed in as {}.", plan_name(&me.plan), printable(&me.status), printable(&me.email))];
    if let Some(end) = me.period_end {
        lines.push(format!("Current period ends {}.", date(end)));
    }
    if me.status == "past_due" {
        lines.push("The last payment didn't go through: update your card with `marv plan --manage`.".into());
    }
    match &me.models {
        Some(models) => {
            lines.push(allowance_line(models));
            let ours = ConfigStore::new(&ctx.config_dir)
                .load()
                .ok()
                .flatten()
                .is_some_and(|f| f.api_key.as_deref() == Some(models.api_key.as_str()));
            if !ours {
                lines.push("Marv isn't using the plan's key: your config has its own OpenRouter key (or none).".into());
            }
        }
        None if me.plan == "pro_models" => {
            lines.push(format!("The model allowance is off while the plan is {}.", printable(&me.status)))
        }
        None => {}
    }
    if verify_licence(&me.licence, &public_key(), ctx.now).is_none() {
        lines.push("The licence didn't check out on this machine, so Marv counts as Free here.".into());
    }
    lines.push("Manage or cancel: `marv plan --manage`.".into());
    (lines.join("\n"), false)
}

/// The billing portal's URL (`POST /portal`). Only https, or http to this machine (a test's stub server).
pub async fn portal_url(ctx: &Ctx) -> Result<reqwest::Url, String> {
    let file = PlanFile::load(&ctx.config_dir).ok_or("You're not signed in to a Marv plan: run `marv login` first.")?;
    let reply = call(&ctx.base, true, "/portal", Some(&file.token), None)
        .await
        .map_err(|e| format!("Couldn't open the billing page: {}.", e.text()))?;
    let url = reply.get("url").and_then(Value::as_str).and_then(|u| reqwest::Url::parse(u).ok());
    let local = |u: &reqwest::Url| matches!(u.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    url.filter(|u| u.scheme() == "https" || (u.scheme() == "http" && local(u)))
        .ok_or_else(|| "The Marv plan server didn't give a billing page Marv can open.".into())
}

/// One line from stdin after `prompt`; `None` at the end of input or for an empty line.
fn ask_stdin(prompt: &str) -> Option<String> {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(n) if n > 0 && !line.trim().is_empty() => Some(line.trim().to_string()),
        _ => None,
    }
}

fn exit_code(result: Result<(), String>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("marv: {e}");
            1
        }
    }
}

/// `marv login [--email <e>]`.
pub async fn run_login(email: Option<String>) -> i32 {
    let ctx = Ctx::from_process();
    let device = device_name(&this_hostname());
    exit_code(login(&ctx, email, &device, &mut ask_stdin, &mut std::io::stdout()).await)
}

/// `marv logout`.
pub async fn run_logout() -> i32 {
    exit_code(logout(&Ctx::from_process(), &mut std::io::stdout()).await)
}

/// `marv plan [--manage]`.
pub async fn run_plan(manage: bool) -> i32 {
    let ctx = Ctx::from_process();
    if manage {
        return exit_code(portal_url(&ctx).await.map(|url| {
            println!("Manage your Marv plan at:\n{}", printable(url.as_str()));
            open(&ctx.env, &url);
        }));
    }
    let (text, is_error) = status(&ctx).await;
    if is_error {
        eprintln!("{text}");
        1
    } else {
        println!("{text}");
        0
    }
}

/// Opens the browser where one can open (not over SSH, not without a display); the link is printed either way.
pub fn open(env: &Env, url: &reqwest::Url) {
    if marv_mcp::oauth::can_open_browser(env, std::env::consts::OS) {
        marv_mcp::oauth::open_browser(url, std::env::consts::OS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// The dev signing key (PKCS#8, base64) from the site's docs: its public half is [`MARV_LICENCE_PUBLIC_KEY`].
    const DEV_SIGNING_KEY: &str = "MC4CAQAwBQYDK2VwBCIEIEm0aG2Nc+lNnN2WE3Bri8UFh/U5NiY8VKGaMRMySHDa";
    const NOW: u64 = 1_780_000_000;
    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const PLAN_KEY: &str = "sk-or-v1-plan0000000000000000000000000000";

    fn dev_key() -> ring::signature::Ed25519KeyPair {
        ring::signature::Ed25519KeyPair::from_pkcs8_maybe_unchecked(&STANDARD.decode(DEV_SIGNING_KEY).unwrap()).unwrap()
    }

    /// A licence as the site signs it.
    fn sign(payload: &Value) -> String {
        let payload = URL_SAFE_NO_PAD.encode(payload.to_string());
        let signature = URL_SAFE_NO_PAD.encode(dev_key().sign(payload.as_bytes()).as_ref());
        format!("{payload}.{signature}")
    }

    fn licence(plan: &str, expires: u64) -> String {
        sign(&json!({"v": 1, "id": "acct_1", "email": "a@b.c", "plan": plan, "issued": NOW - 100, "expires": expires}))
    }

    #[test]
    fn the_compiled_in_key_is_the_dev_signing_keys_public_half() {
        use ring::signature::KeyPair as _;
        assert_eq!(dev_key().public_key().as_ref(), public_key().as_slice());
    }

    #[test]
    fn a_signed_unexpired_licence_holds_and_a_tampered_expired_or_foreign_one_is_free() {
        let key = public_key();
        let good = licence("pro_models", NOW + 1000);
        assert_eq!(
            verify_licence(&good, &key, NOW),
            Some(Licence { plan: "pro_models".into(), email: "a@b.c".into(), expires: NOW + 1000 })
        );
        // Expired: at the expiry second and after.
        assert_eq!(verify_licence(&good, &key, NOW + 1000), None);
        assert_eq!(verify_licence(&licence("founding", NOW - 1), &key, NOW), None);
        // Tampered: another payload under the same signature.
        let (_, signature) = good.split_once('.').unwrap();
        let forged = URL_SAFE_NO_PAD.encode(
            json!({"v": 1, "id": "acct_1", "email": "a@b.c", "plan": "pro_models", "issued": 0, "expires": u64::MAX}).to_string(),
        );
        assert_eq!(verify_licence(&format!("{forged}.{signature}"), &key, NOW), None);
        // A flipped signature byte, another key, another version, garbage.
        let mut bad_sig = good.clone();
        let last = bad_sig.pop().unwrap();
        bad_sig.push(if last == 'A' { 'B' } else { 'A' });
        assert_eq!(verify_licence(&bad_sig, &key, NOW), None);
        assert_eq!(verify_licence(&good, &[7u8; 32], NOW), None);
        let v2 = sign(&json!({"v": 2, "plan": "founding", "expires": NOW + 1000}));
        assert_eq!(verify_licence(&v2, &key, NOW), None);
        for junk in ["", ".", "abc", "a.b.c", "!!!.???"] {
            assert_eq!(verify_licence(junk, &key, NOW), None, "{junk:?}");
        }
    }

    #[test]
    fn the_server_is_the_default_unless_marv_plan_url_says() {
        assert_eq!(base_url(&Env::new()), DEFAULT_URL);
        let env: Env = [("MARV_PLAN_URL".to_string(), " http://127.0.0.1:9/api/ ".to_string())].into();
        assert_eq!(base_url(&env), "http://127.0.0.1:9/api");
    }

    #[test]
    fn dates_device_names_and_labels() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(1_767_225_600), "2026-01-01");
        assert_eq!(date(951_782_400), "2000-02-29");
        assert_eq!(reset_date("2026-12-01T00:00:00Z"), "2026-12-01");
        assert_eq!(reset_date("soon\x1b[2J"), "soon");
        assert_eq!(device_name("box"), "marv cli on box");
        assert_eq!(device_name("\x1b]0;x\x07"), "marv cli on ]0;x");
        assert_eq!(device_name(""), "marv cli on this machine");
        assert_eq!(device_name(&"h".repeat(200)).chars().count(), MAX_DEVICE);
        assert_eq!(key_label(PLAN_KEY), "sk-or-v1-pla…");
        assert!(plausible_email("a@b.c") && !plausible_email("a b@c.d") && !plausible_email("nope") && !plausible_email("a@b"));
    }

    /// A request the stub saw: method and path, Authorization header, body.
    type Seen = Arc<Mutex<Vec<(String, Option<String>, Value)>>>;

    /// A plan server on 127.0.0.1: `answer(method path)` gives the status line and body. Every request is recorded.
    async fn stub(answer: impl Fn(&str) -> (&'static str, String) + Send + Sync + 'static) -> (String, Seen) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen: Seen = Arc::default();
        let (record, answer) = (seen.clone(), Arc::new(answer));
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut data = Vec::new();
                let mut buf = [0u8; 4096];
                // The head, then as much body as Content-Length says.
                let (head, body) = loop {
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    data.extend_from_slice(&buf[..n]);
                    if let Some(at) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&data[..at]).to_string();
                        let length = head
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().to_string()))
                            .and_then(|v| v.parse::<usize>().ok())
                            .unwrap_or(0);
                        while data.len() < at + 4 + length {
                            let n = stream.read(&mut buf).await.unwrap_or(0);
                            if n == 0 {
                                break;
                            }
                            data.extend_from_slice(&buf[..n]);
                        }
                        break (head, data[at + 4..].to_vec());
                    }
                    if n == 0 {
                        break (String::new(), Vec::new());
                    }
                };
                let route =
                    head.lines().next().unwrap_or_default().rsplit_once(' ').map(|(r, _)| r.to_string()).unwrap_or_default();
                let route = route.replacen("/api", "", 1);
                let auth = head.lines().find_map(|l| {
                    l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("authorization")).map(|(_, v)| v.trim().to_string())
                });
                record.lock().unwrap().push((route.clone(), auth, serde_json::from_slice(&body).unwrap_or(Value::Null)));
                let (status, body) = answer(&route);
                let reply = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes()).await;
            }
        });
        (format!("http://{addr}/api"), seen)
    }

    fn me(plan: &str, models: Value) -> String {
        json!({
            "email": "a@b.c", "plan": plan, "status": "active", "periodEnd": 1_767_225_600u64,
            "price": { "amount": 20, "currency": "usd", "interval": "month" },
            "licence": licence(plan, NOW + 86_400), "models": models
        })
        .to_string()
    }

    fn pro_models() -> Value {
        json!({
            "provider": "openrouter", "apiKey": PLAN_KEY, "defaultModel": "deepseek/deepseek-v4-flash",
            "limitUsd": 8, "usedUsd": 1.23, "resetsAt": "2026-12-01T00:00:00Z"
        })
    }

    /// The happy server: every endpoint answers, `/me` with `me_body`.
    fn server(me_body: String) -> impl Fn(&str) -> (&'static str, String) + Send + Sync + 'static {
        move |route| match route {
            "POST /login/start" | "POST /logout" => ("200 OK", r#"{"ok":true}"#.into()),
            "POST /login/verify" => ("200 OK", json!({ "token": TOKEN }).to_string()),
            "GET /me" => ("200 OK", me_body.clone()),
            "POST /portal" => ("200 OK", r#"{"url":"https://billing.stripe.com/p/session/x"}"#.into()),
            _ => ("404 Not Found", r#"{"error":"no such route"}"#.into()),
        }
    }

    fn ctx(base: &str, dir: &Path) -> Ctx {
        Ctx { base: base.into(), config_dir: dir.to_path_buf(), env: Env::new(), now: NOW }
    }

    /// Answers in order; `None` once they run out.
    fn answers(list: &[&str]) -> impl FnMut(&str) -> Option<String> {
        let mut list: Vec<String> = list.iter().rev().map(|s| s.to_string()).collect();
        move |_| list.pop()
    }

    async fn run_login(ctx: &Ctx, email: Option<&str>, typed: &[&str]) -> (Result<(), String>, String) {
        let mut out = Vec::new();
        let result = login(ctx, email.map(Into::into), "marv cli on box", &mut answers(typed), &mut out).await;
        (result, String::from_utf8(out).unwrap())
    }

    #[tokio::test]
    async fn signing_in_to_pro_models_saves_the_plan_and_puts_its_key_to_use() {
        let dir = tempfile::tempdir().unwrap();
        let (base, seen) = stub(server(me("pro_models", pro_models()))).await;
        let (result, out) = run_login(&ctx(&base, dir.path()), None, &["a@b.c", "123 456"]).await;
        result.unwrap();
        assert!(out.contains("Signed in as a@b.c: Marv Pro + models (active)."), "{out}");
        assert!(out.contains("model deepseek/deepseek-v4-flash"), "{out}");
        assert!(out.contains("Model allowance: $1.23 of $8.00 used this month, $6.77 left; it resets on 2026-12-01."), "{out}");

        let seen = seen.lock().unwrap().clone();
        let routes: Vec<&str> = seen.iter().map(|(r, _, _)| r.as_str()).collect();
        assert_eq!(routes, ["POST /login/start", "POST /login/verify", "GET /me"]);
        assert_eq!(seen[0].2, json!({"email": "a@b.c"}));
        assert_eq!(seen[1].2, json!({"email": "a@b.c", "code": "123456", "device": "marv cli on box"}));
        assert_eq!(seen[2].1.as_deref(), Some(format!("Bearer {TOKEN}").as_str()));

        let file = PlanFile::load(dir.path()).unwrap();
        assert_eq!(
            (file.token.as_str(), file.plan.as_str(), file.key_label.as_deref()),
            (TOKEN, "pro_models", Some("sk-or-v1-pla…"))
        );
        let mode = std::fs::metadata(dir.path().join(PLAN_FILE)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(!std::fs::read_to_string(dir.path().join(PLAN_FILE)).unwrap().contains(PLAN_KEY), "never the key itself");
        assert_eq!(licensed_plan(dir.path(), NOW).map(|l| l.plan).as_deref(), Some("pro_models"));

        let config = ConfigStore::new(dir.path()).load().unwrap().unwrap();
        assert_eq!(config.provider, Some(ProviderKind::OpenRouter));
        assert_eq!(config.api_key.as_deref(), Some(PLAN_KEY));
        assert_eq!(config.model.as_deref(), Some("deepseek/deepseek-v4-flash"));
    }

    #[tokio::test]
    async fn signing_in_never_overwrites_the_persons_own_key_or_openrouter_model() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::new(dir.path());
        let own = FileConfig {
            provider: Some(ProviderKind::OpenRouter),
            api_key: Some("sk-or-v1-mine".into()),
            model: Some("my/model".into()),
            ..FileConfig::default()
        };
        store.save(&own).unwrap();
        let (base, _) = stub(server(me("pro_models", pro_models()))).await;
        let (result, out) = run_login(&ctx(&base, dir.path()), Some("a@b.c"), &["123456"]).await;
        result.unwrap();
        assert!(out.contains("You have your own OpenRouter key in config.json, so Marv keeps using it."), "{out}");
        assert_eq!(store.load().unwrap().unwrap(), own);

        // An OpenRouter model of their own, with no key (say OPENROUTER_API_KEY): the key goes in, the model stays.
        store.save(&FileConfig { api_key: None, ..own.clone() }).unwrap();
        run_login(&ctx(&base, dir.path()), Some("a@b.c"), &["123456"]).await.0.unwrap();
        let config = store.load().unwrap().unwrap();
        assert_eq!((config.api_key.as_deref(), config.model.as_deref()), (Some(PLAN_KEY), Some("my/model")));
    }

    #[tokio::test]
    async fn switching_from_another_provider_drops_its_url_and_model() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::new(dir.path());
        store
            .save(&FileConfig {
                provider: Some(ProviderKind::Ollama),
                model: Some("qwen3:8b".into()),
                base_url: Some("http://gpu-box:11434".into()),
                zai_api_key: Some("zai-key".into()),
                ..FileConfig::default()
            })
            .unwrap();
        let (base, _) = stub(server(me("pro_models", pro_models()))).await;
        run_login(&ctx(&base, dir.path()), Some("a@b.c"), &["123456"]).await.0.unwrap();
        let config = store.load().unwrap().unwrap();
        assert_eq!(config.provider, Some(ProviderKind::OpenRouter));
        assert_eq!((config.model.as_deref(), config.base_url.as_deref()), (Some("deepseek/deepseek-v4-flash"), None));
        assert_eq!(config.zai_api_key.as_deref(), Some("zai-key"), "other settings stay");
    }

    #[tokio::test]
    async fn founding_saves_the_plan_and_leaves_the_config_alone() {
        let dir = tempfile::tempdir().unwrap();
        let (base, _) = stub(server(me("founding", Value::Null))).await;
        let (result, out) = run_login(&ctx(&base, dir.path()), Some("a@b.c"), &["123456"]).await;
        result.unwrap();
        assert!(out.contains("Marv Founding (active)") && out.contains("no model allowance"), "{out}");
        assert_eq!(PlanFile::load(dir.path()).unwrap().key_label, None);
        assert!(!ConfigStore::new(dir.path()).path.exists());
    }

    #[tokio::test]
    async fn a_wrong_code_is_said_and_asked_again_and_server_text_is_printable() {
        let dir = tempfile::tempdir().unwrap();
        let tries = Arc::new(Mutex::new(0));
        let me_body = me("founding", Value::Null);
        let (base, _) = stub(move |route| match route {
            "POST /login/verify" => {
                let mut n = tries.lock().unwrap();
                *n += 1;
                if *n == 1 {
                    ("400 Bad Request", r#"{"error":"That code is wrong.\u001b[2J"}"#.into())
                } else {
                    ("200 OK", json!({ "token": TOKEN }).to_string())
                }
            }
            "GET /me" => ("200 OK", me_body.clone()),
            _ => ("200 OK", r#"{"ok":true}"#.into()),
        })
        .await;
        let (result, out) = run_login(&ctx(&base, dir.path()), Some("a@b.c"), &["12345", "111111", "222222"]).await;
        result.unwrap();
        assert!(out.contains("The code is the 6 digits in the email."), "{out}");
        assert!(out.contains("That code is wrong.\n") && !out.contains('\x1b'), "{out:?}");

        // Out of answers: not signed in, nothing saved.
        let dir = tempfile::tempdir().unwrap();
        let (base, _) = stub(|route| match route {
            "POST /login/verify" => ("400 Bad Request", r#"{"error":"Wrong code."}"#.into()),
            _ => ("200 OK", r#"{"ok":true}"#.into()),
        })
        .await;
        let (result, _) = run_login(&ctx(&base, dir.path()), Some("a@b.c"), &["111111"]).await;
        assert!(result.unwrap_err().contains("Not signed in"));
        assert!(PlanFile::load(dir.path()).is_none());
        // A server that's down, and an address that isn't one.
        let (result, _) = run_login(&ctx("http://127.0.0.1:1", dir.path()), Some("a@b.c"), &[]).await;
        assert!(result.unwrap_err().contains("couldn't reach the Marv plan server"));
        let (result, _) = run_login(&ctx(&base, dir.path()), Some("nope"), &[]).await;
        assert!(result.unwrap_err().contains("doesn't look like an email address"));
    }

    async fn signed_in(dir: &Path, base: &str) {
        run_login(&ctx(base, dir), Some("a@b.c"), &["123456"]).await.0.unwrap();
    }

    #[tokio::test]
    async fn signing_out_removes_only_the_plans_key_and_the_plan_file() {
        let dir = tempfile::tempdir().unwrap();
        let (base, seen) = stub(server(me("pro_models", pro_models()))).await;
        signed_in(dir.path(), &base).await;
        let mut out = Vec::new();
        logout(&ctx(&base, dir.path()), &mut out).await.unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("Removed the plan's OpenRouter key"), "{out}");
        assert!(PlanFile::load(dir.path()).is_none() && !dir.path().join(PLAN_FILE).exists());
        let config = ConfigStore::new(dir.path()).load().unwrap().unwrap();
        assert_eq!((config.api_key, config.provider), (None, Some(ProviderKind::OpenRouter)));
        let logout_call = seen.lock().unwrap().iter().find(|(r, _, _)| r == "POST /logout").cloned().unwrap();
        assert_eq!(logout_call.1, Some(format!("Bearer {TOKEN}")));

        // Their own key, set after signing in, stays.
        signed_in(dir.path(), &base).await;
        let store = ConfigStore::new(dir.path());
        store.save(&FileConfig { api_key: Some("sk-or-v1-mine".into()), ..store.load().unwrap().unwrap() }).unwrap();
        logout(&ctx(&base, dir.path()), &mut Vec::new()).await.unwrap();
        assert_eq!(store.load().unwrap().unwrap().api_key.as_deref(), Some("sk-or-v1-mine"));
    }

    #[tokio::test]
    async fn signing_out_offline_still_signs_out_here_but_keeps_a_key_it_cant_confirm() {
        let dir = tempfile::tempdir().unwrap();
        let (base, _) = stub(server(me("pro_models", pro_models()))).await;
        signed_in(dir.path(), &base).await;
        let mut out = Vec::new();
        logout(&ctx("http://127.0.0.1:1", dir.path()), &mut out).await.unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("signing out on this machine anyway") && out.contains("left as it is"), "{out}");
        assert!(PlanFile::load(dir.path()).is_none());
        assert_eq!(ConfigStore::new(dir.path()).load().unwrap().unwrap().api_key.as_deref(), Some(PLAN_KEY));

        let mut out = Vec::new();
        logout(&ctx("http://127.0.0.1:1", dir.path()), &mut out).await.unwrap();
        assert!(String::from_utf8(out).unwrap().contains("not signed in"));
    }

    #[test]
    fn the_price_is_printed_from_its_parts_only() {
        let label = |v: Value| price_label(&v);
        assert_eq!(label(json!({ "amount": 20, "currency": "usd", "interval": "month" })).as_deref(), Some("$20/month"));
        assert_eq!(label(json!({ "amount": 96, "currency": "USD", "interval": "year" })).as_deref(), Some("$96/year"));
        assert_eq!(label(json!({ "amount": 9.5, "currency": "usd", "interval": "month" })).as_deref(), Some("$9.50/month"));
        assert_eq!(label(json!({ "amount": 20, "currency": "eur", "interval": "month" })).as_deref(), Some("20 EUR/month"));
        for bad in [
            json!({ "amount": 20, "currency": "us\u{1b}", "interval": "month" }),
            json!({ "amount": 20, "currency": "usd", "interval": "week" }),
            json!({ "amount": -1, "currency": "usd", "interval": "month" }),
            json!({ "amount": "20", "currency": "usd", "interval": "month" }),
        ] {
            assert_eq!(label(bad.clone()), None, "{bad}");
        }
        // A server older than the field: no price, nothing else changes.
        let old = json!({ "email": "a@b.c", "plan": "founding", "status": "active", "licence": "x" });
        assert_eq!(Me::parse(&old).unwrap().price, None);
    }

    #[tokio::test]
    async fn the_plan_shows_its_state_and_falls_back_to_the_licence_offline() {
        let dir = tempfile::tempdir().unwrap();
        let (text, is_error) = status(&ctx("http://127.0.0.1:1", dir.path())).await;
        assert!(!is_error && text.contains("Marv Free: no account needed"), "{text}");

        let (base, seen) = stub(server(me("pro_models", pro_models()))).await;
        signed_in(dir.path(), &base).await;
        let (text, is_error) = status(&ctx(&base, dir.path())).await;
        assert!(!is_error, "{text}");
        assert!(text.contains("Marv Pro + models at $20/month (active), signed in as a@b.c."), "{text}");
        assert!(text.contains("Current period ends 2026-01-01."), "{text}");
        assert!(text.contains("$6.77 left; it resets on 2026-12-01."), "{text}");
        assert!(!text.contains("isn't using the plan's key"), "{text}");
        assert_eq!(seen.lock().unwrap().last().unwrap().1, Some(format!("Bearer {TOKEN}")));

        let (text, is_error) = status(&ctx("http://127.0.0.1:1", dir.path())).await;
        assert!(is_error && text.contains("This machine's licence: Marv Pro + models, valid until"), "{text}");
        let later = Ctx { now: NOW + 10 * 86_400, ..ctx("http://127.0.0.1:1", dir.path()) };
        assert!(status(&later).await.0.contains("isn't valid, so Marv counts as Free"));

        let (revoked, _) = stub(|_| ("401 Unauthorized", r#"{"error":"bad token"}"#.into())).await;
        let (text, is_error) = status(&ctx(&revoked, dir.path())).await;
        assert!(is_error && text.contains("run `marv login` again"), "{text}");
    }

    #[tokio::test]
    async fn the_billing_page_must_be_https() {
        let dir = tempfile::tempdir().unwrap();
        assert!(portal_url(&ctx("http://127.0.0.1:1", dir.path())).await.unwrap_err().contains("not signed in"));
        let (base, seen) = stub(server(me("founding", Value::Null))).await;
        signed_in(dir.path(), &base).await;
        assert_eq!(portal_url(&ctx(&base, dir.path())).await.unwrap().as_str(), "https://billing.stripe.com/p/session/x");
        assert_eq!(seen.lock().unwrap().last().unwrap().1, Some(format!("Bearer {TOKEN}")));
        for bad in ["javascript:alert(1)", "http://evil.example/x", "file:///etc/passwd", "not a url"] {
            let body = json!({ "url": bad }).to_string();
            let (evil, _) = stub(move |_| ("200 OK", body.clone())).await;
            assert!(portal_url(&ctx(&evil, dir.path())).await.is_err(), "{bad}");
        }
    }
}
