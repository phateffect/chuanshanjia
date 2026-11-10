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

/// 通过 chuanshanjia 后端查询日志。
#[derive(Cli)]
#[usage(bin = "chuanshanjia", version = "0.1.0", arg_required_else_help)]
struct Chuanshanjia {
    #[usage(subcommand)]
    command: Commands,
}

#[derive(Subcommands)]
enum Commands {
    /// 列出后端支持的 service。
    Services(ServicesArgs),
    /// 按 sellerNick、sellerId、storeId 或 leyanSellerId 查卖家。
    Sellers(SellersArgs),
    /// 运行指定的 exam，按买家查询诊断数据。
    Exam(ExamArgs),
    /// 查询指定的后端 service。
    Search(SearchArgs),
    /// 通过 OIDC issuer 登录并保存 token。
    Login(LoginArgs),
    /// 登出：在 IdP 吐销 token 并删除本地凭证。
    Logout(LogoutArgs),
    /// 按需创建配置文件并用 EDITOR 打开。
    Config,
}

#[derive(Args)]
struct ServicesArgs {
    /// Bearer JWT；覆盖配置文件中的 token。
    #[usage(long)]
    token: Option<String>,
}

#[derive(Args)]
struct SellersArgs {
    /// sellerNick、sellerId、storeId 或 leyanSellerId。
    seller: String,

    /// 限定平台；sellerNick 必填。
    #[usage(long)]
    platform: Option<String>,

    /// Bearer JWT；覆盖配置文件中的 token。
    #[usage(long)]
    token: Option<String>,
}

#[derive(Args)]
struct ExamArgs {
    /// exam 名称，如 mixer-config-exam。
    exam: String,

    /// 买家的 leyanBuyerId，必须是 UUID。
    leyan_buyer_id: String,

    /// Bearer JWT；覆盖配置文件中的 token。
    #[usage(long)]
    token: Option<String>,
}

#[derive(Args)]
struct LogoutArgs {
    /// 只删除本地凭证，保留 IdP 上的会话。
    #[usage(long)]
    local: bool,
}

#[derive(Args)]
struct LoginArgs {
    /// OIDC issuer；默认取配置文件中的 `issuer`。
    #[usage(long)]
    issuer: Option<String>,

    /// OIDC client id；默认取配置文件中的 `client_id`。
    #[usage(long)]
    client_id: Option<String>,

    /// Loopback redirect URI；必须与客户端注册完全一致。
    #[usage(long)]
    redirect_uri: Option<String>,

    /// 请求的 scope；默认取配置文件中的 `scope`，再退回
    /// `openid offline_access`。业务 scope 由 IdP 按 `resource` 授予。
    #[usage(long)]
    scope: Option<String>,

    /// token 对应的 resource（RFC 8707）；配置文件中未设置时必填。
    #[usage(long)]
    resource: Option<String>,

    /// 只打印授权 URL，不打开浏览器。
    #[usage(long)]
    no_browser: bool,
}

#[derive(Args)]
#[usage(group("query", required, multiple), args_override_self = false)]
struct SearchArgs {
    /// 要查询的 service 名称。
    service: String,

    /// KEY=VALUE 形式的等值过滤条件；可重复。
    #[usage(short = 'f', long, group = "query", value_name = "KEY=VALUE")]
    filter: Vec<Filter>,

    /// 全文关键词；最多出现一次。
    #[usage(short = 'k', long, group = "query")]
    keywords: Option<String>,

    /// Bearer JWT；覆盖配置文件中的 token。
    #[usage(long)]
    token: Option<String>,

    /// 开始时间：带时区的 ISO 8601、本地（+08:00）时间或 epoch 秒。
    #[usage(long, default_fn = default_start, default_note = "今天 00:00:00 +08:00")]
    start: Timestamp,

    /// 结束时间：带时区的 ISO 8601、本地（+08:00）时间或 epoch 秒。
    #[usage(long, default_fn = default_end, default_note = "当前时间")]
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
            "时间格式无效：{value:?}；请使用带时区的 ISO 8601、YYYY-MM-DD HH:mm:ss（+08:00）或 epoch 秒"
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
            .ok_or_else(|| "filter 必须写成 KEY=VALUE，且 key 不能为空".to_owned())?;
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
    let home = home_dir().context("无法确定用户主目录")?;
    Ok(home.join(".config").join("chuanshanjia"))
}

fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

fn read_config(path: &Path) -> Result<Config> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("读取配置文件 {} 失败", path.display()))?;
    toml::from_str(&contents).with_context(|| format!("解析配置文件 {} 失败", path.display()))
}

fn ensure_config(path: &Path) -> Result<()> {
    let parent = path.parent().context("配置文件路径没有父目录")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("创建配置目录 {} 失败", parent.display()))?;

    let mut file = match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("创建配置文件 {} 失败", path.display()));
        }
    };
    file.write_all(DEFAULT_CONFIG.as_bytes())
        .with_context(|| format!("写入默认配置到 {} 失败", path.display()))
}

fn edit_config() -> Result<()> {
    let path = config_path()?;
    ensure_config(&path)?;
    open_editor(&path)
}

fn open_editor(path: &Path) -> Result<()> {
    let editor = env::var("EDITOR").context("EDITOR 未设置，请设置为你的编辑器命令")?;
    let parts = shell_words::split(&editor).context("解析 EDITOR 失败")?;
    let (program, args) = parts.split_first().context("EDITOR 不能为空")?;
    let status = Command::new(program)
        .args(args)
        .arg(path)
        .status()
        .with_context(|| format!("启动编辑器命令 `{editor}` 失败"))?;

    if !status.success() {
        anyhow::bail!("编辑器命令退出状态为 {status}");
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
            .context("本地登录会话无法续期，请重新执行 `chuanshanjia login`")?;
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
    ensure!(!client_id.trim().is_empty(), "client_id 不能为空");
    if let Some(scope) = scope {
        ensure!(!scope.trim().is_empty(), "设置 scope 时不能为空");
    }
    let issuer_url = reqwest::Url::parse(issuer).context("issuer 无效")?;
    ensure!(
        matches!(issuer_url.scheme(), "http" | "https"),
        "issuer 必须使用 http 或 https"
    );
    ensure!(!resource.trim().is_empty(), "resource 不能为空");
    let resource_url = reqwest::Url::parse(resource).context("resource 无效")?;
    ensure!(
        resource_url.fragment().is_none(),
        "resource 不能包含 fragment"
    );
    Ok(())
}

fn login(args: LoginArgs) -> Result<()> {
    let path = config_path()?;
    let config = read_config(&path)?;
    let issuer = args
        .issuer
        .or(config.issuer)
        .context("请在配置文件中设置 `issuer`，或传入 --issuer")?;
    let client_id = args
        .client_id
        .or(config.client_id)
        .context("请在配置文件中设置 `client_id`，或传入 --client-id")?;
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
        .context("请在配置文件中设置 `resource`，或传入 --resource")?;
    check_login_config(&issuer, &client_id, Some(&scope), &resource)
        .with_context(|| format!("登录配置检查失败：{}", path.display()))?;
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
        "已登录 {issuer}（client {client_id}），token 保存在 {}",
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
            Ok(endpoint) => emit(format!("已吐销 {endpoint} 上的会话")),
            Err(error) => emit(format!("警告：无法吐销会话（{error:#}），仅删除本地凭证")),
        }
    }
    match fs::remove_file(&path) {
        Ok(()) => emit(format!("已删除 {}", path.display())),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            emit(format!("没有本地会话：{}", path.display()));
        }
        Err(error) => {
            return Err(error).with_context(|| format!("删除凭证 {} 失败", path.display()));
        }
    }
    Ok(())
}

fn backend_url(base_url: &str, path: &[&str]) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(base_url)?;
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "base_url 必须使用 http 或 https"
    );
    let mut segments = url
        .path_segments_mut()
        .map_err(|_| anyhow::anyhow!("base_url 不能作为基础 URL 使用"))?;
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

fn http_client(url: &reqwest::Url) -> Result<reqwest::blocking::Client> {
    let mut client = reqwest::blocking::Client::builder();
    if matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "::1" | "[::1]")
    ) {
        client = client.no_proxy();
    }
    client.build().context("构建 HTTP 客户端失败")
}

fn print_json(body: &str) -> Result<()> {
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(body) => emit(serde_json::to_string_pretty(&body)?),
        Err(_) => emit(body),
    }
    Ok(())
}

fn http_get(url: &reqwest::Url, token: Option<String>, label: &str) -> Result<String> {
    let client = http_client(url)?;
    let request = client.get(url.clone());
    let request = match token {
        Some(token) => request.bearer_auth(token),
        None => request,
    };
    let response = request
        .send()
        .with_context(|| format!("GET 请求 {url} 失败"))?;
    let status = response.status();
    let body = response
        .text()
        .with_context(|| format!("读取响应失败：{url}"))?;
    ensure!(
        status.is_success(),
        "{label} 请求失败，HTTP {status}：{body}"
    );
    Ok(body)
}

/// leyanSellerId is a canonical UUID; only its shape matters here.
fn is_uuid(value: &str) -> bool {
    let mut groups = value.split('-');
    [8, 4, 4, 4, 12].iter().all(|&width| {
        groups.next().is_some_and(|group| {
            group.len() == width && group.chars().all(|c| c.is_ascii_hexdigit())
        })
    }) && groups.next().is_none()
}

/// Maps SELLER to the `/sellers` query: ASCII digits are a sellerId/storeId,
/// a UUID is a leyanSellerId, and anything else is a sellerNick, which the
/// backend only accepts together with a platform.
fn seller_query(seller: &str, platform: Option<&str>) -> Result<Vec<(&'static str, String)>> {
    ensure!(!seller.trim().is_empty(), "seller 参数不能为空");
    let key = if seller.chars().all(|c| c.is_ascii_digit()) {
        "seller_id"
    } else if is_uuid(seller) {
        "leyan_seller_id"
    } else {
        ensure!(
            platform.is_some_and(|platform| !platform.trim().is_empty()),
            "{seller:?} 会按 sellerNick 查询，请用 --platform 限定平台"
        );
        "seller_nick"
    };
    let mut params = Vec::new();
    if let Some(platform) = platform.filter(|platform| !platform.trim().is_empty()) {
        params.push(("platform", platform.to_owned()));
    }
    params.push((key, seller.to_owned()));
    Ok(params)
}

fn services(args: ServicesArgs) -> Result<()> {
    let path = config_path()?;
    let config = read_config(&path)?;
    let url = backend_url(&config.base_url, &["services"])
        .with_context(|| format!("配置文件 {} 中的 base_url 无效", path.display()))?;
    let token = resolve_token(args.token, &config)?;
    print_json(&http_get(&url, token, "services")?)
}

fn sellers(args: SellersArgs) -> Result<()> {
    let params = seller_query(&args.seller, args.platform.as_deref())?;
    let path = config_path()?;
    let config = read_config(&path)?;
    let mut url = backend_url(&config.base_url, &["sellers"])
        .with_context(|| format!("配置文件 {} 中的 base_url 无效", path.display()))?;
    let token = resolve_token(args.token, &config)?;
    url.query_pairs_mut().extend_pairs(params);
    print_json(&http_get(&url, token, "sellers")?)
}

fn exam(args: ExamArgs) -> Result<()> {
    ensure!(!args.exam.trim().is_empty(), "exam 名称不能为空");
    ensure!(
        is_uuid(&args.leyan_buyer_id),
        "leyanBuyerId 必须是 UUID，得到 {:?}",
        args.leyan_buyer_id
    );
    let path = config_path()?;
    let config = read_config(&path)?;
    let url = backend_url(
        &config.base_url,
        &["api", "exams", &args.exam, &args.leyan_buyer_id],
    )
    .with_context(|| format!("配置文件 {} 中的 base_url 无效", path.display()))?;
    let token = resolve_token(args.token, &config)?;
    print_json(&http_get(&url, token, "exam")?)
}

fn search(args: SearchArgs) -> Result<()> {
    ensure!(args.start.0 < args.end.0, "start 必须早于 end");

    let path = config_path()?;
    let config = read_config(&path)?;
    let url = search_url(&config.base_url, &args.service)
        .with_context(|| format!("配置文件 {} 中的 base_url 无效", path.display()))?;
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

    let client = http_client(&url)?;
    let request = client.post(url.clone()).json(&body);
    let request = match token {
        Some(token) => request.bearer_auth(token),
        None => request,
    };
    let response = request
        .send()
        .with_context(|| format!("POST 请求 {url} 失败"))?;
    let status = response.status();
    let body = response
        .text()
        .with_context(|| format!("读取响应失败：{url}"))?;
    ensure!(
        status.is_success(),
        "search 请求失败，HTTP {status}：{body}"
    );

    print_json(&body)
}

fn run() -> Result<()> {
    match Chuanshanjia::parse().command {
        Commands::Services(args) => services(args),
        Commands::Sellers(args) => sellers(args),
        Commands::Exam(args) => exam(args),
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

    #[test]
    fn seller_query_maps_each_identifier_shape() {
        let nick = seller_query("shop", Some("taobao")).unwrap();
        let id = seller_query("123456", Some("taobao")).unwrap();
        let store = seller_query("123456", None).unwrap();
        let leyan = seller_query("3fa85f64-5717-4562-b3fc-2c963f66afa6", None).unwrap();

        assert_eq!(
            nick,
            [
                ("platform", "taobao".to_owned()),
                ("seller_nick", "shop".to_owned()),
            ]
        );
        assert_eq!(
            id,
            [
                ("platform", "taobao".to_owned()),
                ("seller_id", "123456".to_owned()),
            ]
        );
        assert_eq!(store, [("seller_id", "123456".to_owned())]);
        assert_eq!(
            leyan,
            [(
                "leyan_seller_id",
                "3fa85f64-5717-4562-b3fc-2c963f66afa6".to_owned()
            )]
        );
    }

    #[test]
    fn seller_query_requires_platform_for_a_nick_and_rejects_an_empty_seller() {
        let error = seller_query("shop", None).unwrap_err().to_string();
        assert!(error.contains("--platform"), "unexpected error: {error}");
        assert!(seller_query("  ", Some("taobao")).is_err());
    }

    #[test]
    fn uuid_detection_requires_the_canonical_shape() {
        assert!(is_uuid("3fa85f64-5717-4562-b3fc-2c963f66afa6"));
        assert!(is_uuid("3FA85F64-5717-4562-B3FC-2C963F66AFA6"));
        for value in [
            "",
            "3fa85f64-5717-4562-b3fc-2c963f66afa",
            "3fa85f645717-4562-b3fc-2c963f66afa6",
            "3fa85f64-5717-4562-b3fc-2c963f66afa6-",
            "3fa85f64-5717-4562-b3fc-2c963f66afzz",
            "123456",
        ] {
            assert!(!is_uuid(value), "accepted {value:?}");
        }
    }
}
