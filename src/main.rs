use std::collections::BTreeMap;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, FixedOffset, NaiveDateTime, Utc};
use serde::Deserialize;
use usage_rs::{Args, Cli, Subcommands};

mod oidc;

/// Print a line to stdout, ignoring a closed pipe (`| head`, piped tests) instead
/// of panicking the way `println!` does on EPIPE.
pub(crate) fn emit(line: impl std::fmt::Display) {
    use std::io::Write;
    let mut stdout = std::io::stdout();
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
}

// RFC 8252: the IdP ignores the port of a loopback redirect URI, so ask the OS
// for a free one instead of squatting on a fixed port.
const DEFAULT_REDIRECT_URI: &str = "http://127.0.0.1:0/callback";
// Standard OIDC scopes only: `openid` identifies the login, `offline_access`
// makes the IdP issue a refresh token. Business scopes are deliberately not
// requested; the IdP derives them from the resource (RFC 8707).
const DEFAULT_SCOPE: &str = "openid offline_access";
const DEFAULT_CONFIG: &str = "base_url = \"\"\n# token = \"\"\n# issuer = \"https://oidc.example.com\"\n# client_id = \"chuanshanjia\"\nresource = \"\"\nscope = \"openid offline_access\"\n";

/// Search logs through a chuanshanjia backend.
#[derive(Cli)]
#[usage(bin = "chuanshanjia", version = "0.1.0", arg_required_else_help)]
struct Chuanshanjia {
    #[usage(subcommand)]
    command: Commands,
}

#[derive(Subcommands)]
enum Commands {
    /// List services supported by the backend.
    Services(ServicesArgs),
    /// Search a configured backend service.
    Search(SearchArgs),
    /// Log in through the OIDC issuer and store the tokens.
    Login(LoginArgs),
    /// Log out: revoke the tokens at the IdP and remove them locally.
    Logout(LogoutArgs),
    /// Create the config file if needed and open it with EDITOR.
    Config,
}

#[derive(Args)]
struct ServicesArgs {
    /// Bearer JWT; overrides token from the config file.
    #[usage(long)]
    token: Option<String>,
}

#[derive(Args)]
struct LogoutArgs {
    /// Only delete the local credentials; keep the session valid at the IdP.
    #[usage(long)]
    local: bool,
}

#[derive(Args)]
struct LoginArgs {
    /// OIDC issuer; defaults to `issuer` in the config file.
    #[usage(long)]
    issuer: Option<String>,

    /// OIDC client id; defaults to `client_id` in the config file.
    #[usage(long)]
    client_id: Option<String>,

    /// Loopback redirect URI; must match the client registration exactly.
    #[usage(long)]
    redirect_uri: Option<String>,

    /// Scopes to request; defaults to `scope` in the config, then
    /// `openid offline_access`. Business scopes come from the IdP via `resource`.
    #[usage(long)]
    scope: Option<String>,

    /// Resource the token is for (RFC 8707); required unless set in the config.
    #[usage(long)]
    resource: Option<String>,

    /// Print the authorization URL instead of opening a browser.
    #[usage(long)]
    no_browser: bool,
}

#[derive(Args)]
#[usage(group("query", required, multiple), args_override_self = false)]
struct SearchArgs {
    /// Name of the configured search service.
    service: String,

    /// Exact-match filter in KEY=VALUE form; may be repeated.
    #[usage(short = 'f', long, group = "query", value_name = "KEY=VALUE")]
    filter: Vec<Filter>,

    /// Full-text keywords; may be supplied at most once.
    #[usage(short = 'k', long, group = "query")]
    keywords: Option<String>,

    /// Bearer JWT; overrides token from the config file.
    #[usage(long)]
    token: Option<String>,

    /// Start time: ISO 8601 with timezone, local (+08:00) datetime, or epoch seconds.
    #[usage(long, default_fn = default_start, default_note = "today at 00:00:00 +08:00")]
    start: Timestamp,

    /// End time: ISO 8601 with timezone, local (+08:00) datetime, or epoch seconds.
    #[usage(long, default_fn = default_end, default_note = "now")]
    end: Timestamp,
}

#[derive(Clone, Copy)]
struct Timestamp(i64);

impl std::fmt::Display for Timestamp {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

impl FromStr for Timestamp {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        if let Ok(seconds) = value.parse() {
            return Ok(Self(seconds));
        }
        if let Ok(datetime) = DateTime::parse_from_rfc3339(value)
            .or_else(|_| DateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f%z"))
        {
            return Ok(Self(datetime.timestamp()));
        }
        if let Ok(datetime) = NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
            && let Some(datetime) = datetime.and_local_timezone(local_timezone()).single()
        {
            return Ok(Self(datetime.timestamp()));
        }

        Err(format!(
            "invalid timestamp {value:?}; expected ISO 8601 with timezone, YYYY-MM-DD HH:mm:ss (+08:00), or epoch seconds"
        ))
    }
}

struct Filter {
    key: String,
    value: String,
}

impl FromStr for Filter {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let (key, value) = value
            .split_once('=')
            .filter(|(key, _)| !key.is_empty())
            .ok_or_else(|| "filter must use KEY=VALUE with a non-empty key".to_owned())?;
        Ok(Self {
            key: key.to_owned(),
            value: value.to_owned(),
        })
    }
}

fn local_timezone() -> FixedOffset {
    FixedOffset::east_opt(8 * 3600).expect("UTC+08:00 is a valid fixed offset")
}

fn start_of_day(now: DateTime<Utc>) -> Timestamp {
    let today = now.with_timezone(&local_timezone()).date_naive();
    let midnight = today
        .and_hms_opt(0, 0, 0)
        .expect("midnight is a valid local time");
    let datetime = midnight
        .and_local_timezone(local_timezone())
        .single()
        .expect("a fixed offset has no ambiguous local times");
    Timestamp(datetime.timestamp())
}

fn default_start() -> Timestamp {
    start_of_day(Utc::now())
}

fn default_end() -> Timestamp {
    Timestamp(Utc::now().timestamp())
}

#[derive(Deserialize)]
struct Config {
    base_url: String,
    token: Option<String>,
    issuer: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    resource: Option<String>,
    scope: Option<String>,
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    if let Some(profile) = std::env::var_os("USERPROFILE").filter(|profile| !profile.is_empty()) {
        return Some(PathBuf::from(profile));
    }

    dirs::home_dir()
}

fn config_dir() -> Result<PathBuf> {
    let home = home_dir().context("could not determine the user home directory")?;
    Ok(home.join(".config").join("chuanshanjia"))
}

fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

fn read_config(path: &Path) -> Result<Config> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    toml::from_str(&contents)
        .with_context(|| format!("failed to parse config file {}", path.display()))
}

fn ensure_config(path: &Path) -> Result<()> {
    let parent = path.parent().context("config file path has no parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create config directory {}", parent.display()))?;

    let mut file = match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to create config file {}", path.display()));
        }
    };
    file.write_all(DEFAULT_CONFIG.as_bytes())
        .with_context(|| format!("failed to write default config to {}", path.display()))
}

fn edit_config() -> Result<()> {
    let path = config_path()?;
    ensure_config(&path)?;
    open_editor(&path)
}

fn open_editor(path: &Path) -> Result<()> {
    let editor = env::var("EDITOR").context("EDITOR is not set; set it to your editor command")?;
    let parts = shell_words::split(&editor).context("failed to parse EDITOR")?;
    let (program, args) = parts.split_first().context("EDITOR cannot be empty")?;
    let status = Command::new(program)
        .args(args)
        .arg(path)
        .status()
        .with_context(|| format!("failed to start EDITOR command `{editor}`"))?;

    if !status.success() {
        anyhow::bail!("EDITOR command exited with {status}");
    }
    Ok(())
}

fn credentials_path() -> Result<PathBuf> {
    Ok(oidc::credentials_path(&config_dir()?))
}

/// Resolve a Bearer token: `--token`, then stored OIDC credentials (refreshed
/// when expired), then a plain `token` in the config file.
fn resolve_token(explicit: Option<String>, config: &Config) -> Result<Option<String>> {
    if let Some(token) = explicit {
        return Ok(Some(token));
    }
    match load_stored_token()? {
        Some(token) => Ok(Some(token)),
        None => Ok(config.token.clone()),
    }
}

fn load_stored_token() -> Result<Option<String>> {
    let path = credentials_path()?;
    let Some(credentials) = oidc::load(&path)? else {
        return Ok(None);
    };
    let credentials = if oidc::is_fresh(&credentials, oidc::now()) {
        credentials
    } else {
        let refreshed = oidc::refresh(&credentials)
            .context("the stored session could not be refreshed; run `chuanshanjia login` again")?;
        oidc::store(&path, &refreshed)?;
        refreshed
    };
    Ok(Some(oidc::bearer_token(&credentials)?.to_owned()))
}

fn check_login_config(
    issuer: &str,
    client_id: &str,
    scope: Option<&str>,
    resource: &str,
) -> Result<()> {
    ensure!(!client_id.trim().is_empty(), "client_id must not be empty");
    if let Some(scope) = scope {
        ensure!(!scope.trim().is_empty(), "scope must not be empty when set");
    }
    let issuer_url = reqwest::Url::parse(issuer).context("invalid issuer")?;
    ensure!(
        matches!(issuer_url.scheme(), "http" | "https"),
        "issuer must use http or https"
    );
    ensure!(!resource.trim().is_empty(), "resource must not be empty");
    let resource_url = reqwest::Url::parse(resource).context("invalid resource")?;
    ensure!(
        resource_url.fragment().is_none(),
        "resource must not contain a fragment"
    );
    Ok(())
}

fn login(args: LoginArgs) -> Result<()> {
    let path = config_path()?;
    let config = read_config(&path)?;
    let issuer = args
        .issuer
        .or(config.issuer)
        .context("set `issuer` in the config file or pass --issuer")?;
    let client_id = args
        .client_id
        .or(config.client_id)
        .context("set `client_id` in the config file or pass --client-id")?;
    let scope = args
        .scope
        .or(config.scope.clone())
        .unwrap_or_else(|| DEFAULT_SCOPE.to_owned());

    let redirect_uri = args
        .redirect_uri
        .or(config.redirect_uri.clone())
        .unwrap_or_else(|| DEFAULT_REDIRECT_URI.to_owned());
    let resource = args
        .resource
        .or(config.resource.clone())
        .context("set `resource` in the config file or pass --resource")?;
    check_login_config(&issuer, &client_id, Some(&scope), &resource)
        .with_context(|| format!("login configuration check failed for {}", path.display()))?;
    let credentials = oidc::login(
        &issuer,
        &client_id,
        Some(&scope),
        &redirect_uri,
        &[resource],
        !args.no_browser,
    )?;
    let path = credentials_path()?;
    oidc::store(&path, &credentials)?;
    emit(format!(
        "Logged in to {issuer} as client {client_id}; tokens saved to {}",
        path.display()
    ));
    Ok(())
}

fn logout(args: LogoutArgs) -> Result<()> {
    let path = credentials_path()?;
    if !args.local
        && let Some(credentials) = oidc::load(&path)?
    {
        match oidc::revoke(&credentials) {
            Ok(endpoint) => emit(format!("Revoked the session at {endpoint}")),
            Err(error) => emit(format!(
                "Warning: could not revoke the session ({error:#}); removing local credentials only"
            )),
        }
    }
    match fs::remove_file(&path) {
        Ok(()) => emit(format!("Removed {}", path.display())),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            emit(format!("No stored session at {}", path.display()));
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to remove credentials {}", path.display()));
        }
    }
    Ok(())
}

fn backend_url(base_url: &str, path: &[&str]) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(base_url)?;
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "base_url must use http or https"
    );
    let mut segments = url
        .path_segments_mut()
        .map_err(|_| anyhow::anyhow!("base_url cannot be used as a base URL"))?;
    segments.pop_if_empty();
    for segment in path {
        segments.push(segment);
    }
    drop(segments);
    Ok(url)
}

fn search_url(base_url: &str, service: &str) -> Result<reqwest::Url> {
    backend_url(base_url, &[service, "_search"])
}

fn services(args: ServicesArgs) -> Result<()> {
    let path = config_path()?;
    let config = read_config(&path)?;
    let url = backend_url(&config.base_url, &["services"])
        .with_context(|| format!("invalid base_url in {}", path.display()))?;
    let token = resolve_token(args.token, &config)?;

    let mut client = reqwest::blocking::Client::builder();
    if matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "::1" | "[::1]")
    ) {
        client = client.no_proxy();
    }
    let client = client.build().context("failed to build HTTP client")?;
    let request = client.get(url.clone());
    let request = match token {
        Some(token) => request.bearer_auth(token),
        None => request,
    };
    let response = request
        .send()
        .with_context(|| format!("failed to GET {url}"))?;
    let status = response.status();
    let body = response
        .text()
        .with_context(|| format!("failed to read response from {url}"))?;
    ensure!(
        status.is_success(),
        "services request failed with {status}: {body}"
    );

    match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(body) => emit(serde_json::to_string_pretty(&body)?),
        Err(_) => emit(body),
    }
    Ok(())
}

fn search(args: SearchArgs) -> Result<()> {
    ensure!(args.start.0 < args.end.0, "start must be earlier than end");

    let path = config_path()?;
    let config = read_config(&path)?;
    let url = search_url(&config.base_url, &args.service)
        .with_context(|| format!("invalid base_url in {}", path.display()))?;
    let token = resolve_token(args.token, &config)?;
    let filter = args
        .filter
        .into_iter()
        .map(|filter| (filter.key, filter.value))
        .collect::<BTreeMap<_, _>>();
    let mut body = serde_json::Map::new();
    body.insert("filter".to_owned(), serde_json::json!(filter));
    if let Some(keywords) = args.keywords {
        body.insert("keywords".to_owned(), keywords.into());
    }
    body.insert("start".to_owned(), args.start.0.into());
    body.insert("end".to_owned(), args.end.0.into());
    let body = serde_json::Value::Object(body);

    let mut client = reqwest::blocking::Client::builder();
    if matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "::1" | "[::1]")
    ) {
        client = client.no_proxy();
    }
    let client = client.build().context("failed to build HTTP client")?;
    let request = client.post(url.clone()).json(&body);
    let request = match token {
        Some(token) => request.bearer_auth(token),
        None => request,
    };
    let response = request
        .send()
        .with_context(|| format!("failed to POST to {url}"))?;
    let status = response.status();
    let body = response
        .text()
        .with_context(|| format!("failed to read response from {url}"))?;
    ensure!(
        status.is_success(),
        "search request failed with {status}: {body}"
    );

    match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(body) => emit(serde_json::to_string_pretty(&body)?),
        Err(_) => emit(body),
    }
    Ok(())
}

fn run() -> Result<()> {
    match Chuanshanjia::parse().command {
        Commands::Services(args) => services(args),
        Commands::Search(args) => search(args),
        Commands::Login(args) => login(args),
        Commands::Logout(args) => logout(args),
        Commands::Config => edit_config(),
    }
}

fn main() -> Result<()> {
    run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_accepts_iso8601_local_datetime_and_epoch_seconds() {
        let local = "2026-10-14 10:01:02".parse::<Timestamp>().unwrap();
        let offset = "2026-10-14T10:01:02+08:00".parse::<Timestamp>().unwrap();
        let utc = "2026-10-14T02:01:02Z".parse::<Timestamp>().unwrap();
        let epoch = local.0.to_string().parse::<Timestamp>().unwrap();

        assert_eq!(local.0, offset.0);
        assert_eq!(local.0, utc.0);
        assert_eq!(local.0, epoch.0);
    }

    #[test]
    fn timestamp_requires_a_valid_datetime_and_timezone() {
        for value in ["", "not-a-time", "2026-10-14T10:01:02"] {
            assert!(value.parse::<Timestamp>().is_err(), "accepted {value:?}");
        }
    }

    #[test]
    fn default_start_uses_the_utc_plus_eight_calendar_day() {
        let now = DateTime::parse_from_rfc3339("2026-10-14T16:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let expected = DateTime::parse_from_rfc3339("2026-10-14T16:00:00Z")
            .unwrap()
            .timestamp();

        assert_eq!(start_of_day(now).0, expected);
    }

    #[test]
    fn search_arguments_use_dynamic_time_defaults() {
        let earliest_start = default_start().0;
        let before = Utc::now().timestamp();
        let argv = ["search", "foo", "-k", "timeout"].map(std::ffi::OsStr::new);
        let cli = Chuanshanjia::parse_from(&argv).unwrap();
        let after = Utc::now().timestamp();
        let latest_start = default_start().0;
        let Commands::Search(args) = cli.command else {
            panic!("expected search command");
        };

        assert!(args.start.0 == earliest_start || args.start.0 == latest_start);
        assert!((before..=after).contains(&args.end.0));
        assert_eq!(args.service, "foo");
        assert_eq!(args.keywords.as_deref(), Some("timeout"));
    }

    #[test]
    fn default_scope_requests_only_standard_oidc_scopes() {
        assert_eq!(DEFAULT_SCOPE, "openid offline_access");
        for business_like in ["chuanshanjia:read", "logs:read"] {
            assert!(!DEFAULT_SCOPE.contains(business_like));
        }
    }
}
