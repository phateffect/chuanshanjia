//! OIDC login for the CLI: authorization code flow with PKCE against a public
//! client, the code delivered to a loopback redirect URI.
//!
//! The backend never issues tokens; it only verifies them. This module is the
//! other half of that contract: it obtains the IdP's tokens and keeps them
//! fresh locally.

use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::random;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);
const EXPIRY_SKEW: i64 = 60;

#[derive(Deserialize)]
struct Discovery {
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    revocation_endpoint: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

#[derive(Deserialize)]
struct OAuthError {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

/// Tokens obtained from the IdP, persisted between CLI invocations.
#[derive(Debug, Serialize, Deserialize)]
pub struct Credentials {
    pub issuer: String,
    pub client_id: String,
    pub token_endpoint: String,
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Epoch seconds at which `access_token` stops being usable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// RFC 7009 revocation endpoint, discovered at login.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revocation_endpoint: Option<String>,
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

pub fn credentials_path(config_dir: &Path) -> PathBuf {
    config_dir.join("credentials.toml")
}

pub fn load(path: &Path) -> Result<Option<Credentials>> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read credentials {}", path.display()));
        }
    };
    let credentials = toml::from_str(&contents)
        .with_context(|| format!("failed to parse credentials {}", path.display()))?;
    Ok(Some(credentials))
}

pub fn store(path: &Path, credentials: &Credentials) -> Result<()> {
    let parent = path.parent().context("credentials path has no parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create config directory {}", parent.display()))?;
    let contents = toml::to_string(credentials).context("failed to serialize credentials")?;
    write_private(path, contents.as_bytes())
        .with_context(|| format!("failed to write credentials {}", path.display()))
}

#[cfg(unix)]
fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents)?;
    // An existing file keeps its old mode, so enforce it explicitly.
    fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    fs::write(path, contents)
}

pub(crate) fn client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client")
}

fn discover(http: &reqwest::blocking::Client, issuer: &str) -> Result<(Discovery, Url)> {
    let issuer = issuer.trim_end_matches('/').to_owned();
    let url = Url::parse(&issuer).with_context(|| format!("invalid issuer {issuer:?}"))?;
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "issuer must use http or https"
    );
    let document_url = Url::parse(&format!("{issuer}/.well-known/openid-configuration"))?;
    let response = http
        .get(document_url.clone())
        .send()
        .with_context(|| format!("failed to GET {document_url}"))?;
    let status = response.status();
    let body = response
        .text()
        .context("failed to read the discovery response")?;
    ensure!(
        status.is_success(),
        "GET {document_url} returned {status}: {body}"
    );
    let discovery: Discovery = serde_json::from_str(&body)
        .with_context(|| format!("invalid discovery document at {document_url}"))?;

    Ok((discovery, Url::parse(&issuer)?))
}

/// Run the authorization code + PKCE flow and return the resulting tokens.
pub fn login(
    issuer: &str,
    client_id: &str,
    scope: Option<&str>,
    redirect_uri: &str,
    resources: &[String],
    open: bool,
) -> Result<Credentials> {
    let http = client()?;
    let (discovery, issuer_url) = discover(&http, issuer)?;
    let issuer = issuer_url.as_str().trim_end_matches('/').to_owned();

    let redirect = Url::parse(redirect_uri)
        .with_context(|| format!("invalid redirect URI {redirect_uri:?}"))?;
    ensure!(
        redirect.scheme() == "http",
        "redirect URI must use http on the loopback interface"
    );
    ensure!(
        matches!(
            redirect.host_str(),
            Some("127.0.0.1" | "localhost" | "[::1]" | "::1")
        ),
        "redirect URI host must be a loopback address"
    );
    let requested_port = redirect.port_or_known_default().unwrap_or(80);
    let listener = TcpListener::bind(("127.0.0.1", requested_port)).with_context(|| {
        format!(
            "failed to listen on 127.0.0.1:{requested_port} for the login callback; \
             the IdP matches the redirect URI exactly, so register and pass one with a free unprivileged port"
        )
    })?;
    // Port 0 asks the OS for a free port; the redirect URI must then carry it.
    let bound_port = listener
        .local_addr()
        .context("failed to read the loopback callback address")?
        .port();
    let redirect_uri = if bound_port == requested_port {
        redirect_uri.to_owned()
    } else {
        let mut redirect = redirect;
        redirect
            .set_port(Some(bound_port))
            .map_err(|_| anyhow::anyhow!("invalid redirect URI {redirect_uri:?}"))?;
        redirect.to_string()
    };
    let redirect_uri = redirect_uri.as_str();

    let verifier: String = URL_SAFE_NO_PAD.encode(random::<[u8; 32]>());
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state: String = URL_SAFE_NO_PAD.encode(random::<[u8; 16]>());

    let mut authorize = Url::parse(&discovery.authorization_endpoint)
        .context("invalid authorization_endpoint in discovery document")?;
    authorize
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");
    if let Some(scope) = scope {
        authorize.query_pairs_mut().append_pair("scope", scope);
        // IdPs commonly only issue a refresh token when consent is explicit.
        if scope
            .split_whitespace()
            .any(|scope| scope == "offline_access")
        {
            authorize.query_pairs_mut().append_pair("prompt", "consent");
        }
    }
    // RFC 8707: the audience of the access token is the requested resource.
    for resource in resources {
        authorize
            .query_pairs_mut()
            .append_pair("resource", resource);
    }

    crate::emit("Open this URL in a browser to log in:");
    crate::emit(&authorize);
    if open && !open_browser(authorize.as_str()) {
        crate::emit("Could not open a browser automatically; open the URL above manually.");
    }

    let code = wait_for_code(&listener, &state)?;
    let tokens = exchange_code(
        &http,
        &discovery.token_endpoint,
        client_id,
        &code,
        redirect_uri,
        &verifier,
        resources,
    )?;
    let mut credentials =
        credentials_from(issuer, client_id, &discovery.token_endpoint, tokens, None);
    credentials.revocation_endpoint = discovery.revocation_endpoint;
    Ok(credentials)
}

/// Refresh an expired access token, keeping the previous refresh token when the
/// IdP does not rotate it.
pub fn refresh(credentials: &Credentials) -> Result<Credentials> {
    let refresh_token = credentials
        .refresh_token
        .as_deref()
        .context("no refresh token; run `chuanshanjia login` again")?;
    let tokens = post_token(
        &client()?,
        &credentials.token_endpoint,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", credentials.client_id.as_str()),
        ],
    )?;
    Ok(credentials_from(
        credentials.issuer.clone(),
        &credentials.client_id,
        &credentials.token_endpoint,
        tokens,
        Some(credentials),
    ))
}

/// The token to send as a Bearer credential. The backend verifies signatures
/// locally, so only a JWT access token is usable.
pub fn bearer_token(credentials: &Credentials) -> Result<&str> {
    ensure!(
        is_jwt(&credentials.access_token),
        "the identity provider must return a JWT access token; opaque access tokens are not supported"
    );
    Ok(&credentials.access_token)
}

fn is_jwt(token: &str) -> bool {
    let mut parts = token.split('.');
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some(header), Some(payload), Some(signature), None)
            if !header.is_empty() && !payload.is_empty() && !signature.is_empty()
    )
}

/// Revoke the stored tokens at the IdP (RFC 7009). Returns the endpoint used.
///
/// Revocation is best effort: the caller still deletes the local credentials when
/// the IdP is unreachable, so `logout` keeps working offline.
pub fn revoke(credentials: &Credentials) -> Result<String> {
    let endpoint = match &credentials.revocation_endpoint {
        Some(endpoint) => endpoint.clone(),
        None => {
            let (discovery, _) = discover(&client()?, &credentials.issuer)?;
            discovery
                .revocation_endpoint
                .context("the identity provider exposes no revocation endpoint")?
        }
    };
    // Revoking the refresh token cascades to its access tokens per RFC 7009 §2.1.
    let (token, hint) = match credentials.refresh_token.as_deref() {
        Some(refresh_token) => (refresh_token, "refresh_token"),
        None => (credentials.access_token.as_str(), "access_token"),
    };
    let http = client()?;
    let response = http
        .post(&endpoint)
        .form(&[
            ("token", token),
            ("token_type_hint", hint),
            ("client_id", credentials.client_id.as_str()),
        ])
        .send()
        .with_context(|| format!("failed to POST to {endpoint}"))?;
    let status = response.status();
    ensure!(
        status.is_success(),
        "revocation at {endpoint} returned {status}"
    );
    Ok(endpoint)
}

/// Whether the token still has enough lifetime left to be used as is.
pub fn is_fresh(credentials: &Credentials, now: i64) -> bool {
    credentials
        .expires_at
        .is_none_or(|expires_at| expires_at > now + EXPIRY_SKEW)
}

fn credentials_from(
    issuer: String,
    client_id: &str,
    token_endpoint: &str,
    tokens: TokenResponse,
    previous: Option<&Credentials>,
) -> Credentials {
    let previous_refresh = previous.and_then(|previous| previous.refresh_token.clone());
    // Not every IdP returns `expires_in` on the authorization-code exchange.
    // Falling back to the JWT `exp` claim keeps `is_fresh` meaningful, so a
    // short-lived token is actually refreshed instead of being treated as
    // valid forever.
    let expires_at = tokens
        .expires_in
        .map(|expires_in| now() + expires_in)
        .or_else(|| jwt_expiry(&tokens.access_token));
    Credentials {
        issuer,
        client_id: client_id.to_owned(),
        token_endpoint: token_endpoint.to_owned(),
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token.or(previous_refresh),
        expires_at,
        revocation_endpoint: previous.and_then(|previous| previous.revocation_endpoint.clone()),
    }
}

/// Reads the `exp` claim from a JWT access token, if it is present and well
/// formed. Used only to schedule refreshes; the token itself is still verified
/// by the backend.
fn jwt_expiry(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let claims: serde_json::Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
    claims.get("exp")?.as_i64()
}

fn exchange_code(
    http: &reqwest::blocking::Client,
    token_endpoint: &str,
    client_id: &str,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
    resources: &[String],
) -> Result<TokenResponse> {
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
        ("code_verifier", verifier),
    ];
    // RFC 8707 §2.2: repeat the same resources at the token endpoint.
    form.extend(
        resources
            .iter()
            .map(|resource| ("resource", resource.as_str())),
    );
    post_token(http, token_endpoint, &form)
}

fn post_token(
    http: &reqwest::blocking::Client,
    token_endpoint: &str,
    form: &[(&str, &str)],
) -> Result<TokenResponse> {
    let response = http
        .post(token_endpoint)
        .form(form)
        .send()
        .with_context(|| format!("failed to POST to {token_endpoint}"))?;
    let status = response.status();
    let body = response
        .text()
        .context("failed to read the token response")?;
    if !status.is_success() {
        let detail = match serde_json::from_str::<OAuthError>(&body) {
            Ok(error) => match error.error_description {
                Some(description) => format!("{}: {description}", error.error),
                None => error.error,
            },
            Err(_) => body.clone(),
        };
        return Err(anyhow::anyhow!(
            "token request to {token_endpoint} failed with {status}: {detail}"
        ));
    }
    let tokens: TokenResponse = serde_json::from_str(&body)
        .with_context(|| format!("invalid token response from {token_endpoint}"))?;
    ensure!(
        !tokens.access_token.is_empty(),
        "token response from {token_endpoint} has an empty access_token"
    );
    Ok(tokens)
}

fn wait_for_code(listener: &TcpListener, expected_state: &str) -> Result<String> {
    let deadline = std::time::Instant::now() + CALLBACK_TIMEOUT;
    listener
        .set_nonblocking(true)
        .context("failed to configure the loopback listener")?;
    loop {
        match listener.accept() {
            Ok((stream, _)) => return handle_callback(stream, expected_state),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                ensure!(
                    std::time::Instant::now() < deadline,
                    "timed out waiting for the browser callback; run `chuanshanjia login` again"
                );
                thread::sleep(Duration::from_millis(100));
            }
            Err(error) => return Err(error).context("failed to accept the browser callback"),
        }
    }
}

fn handle_callback(mut stream: TcpStream, expected_state: &str) -> Result<String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .context("failed to configure the callback connection")?;
    let mut buffer = [0u8; 8192];
    let read = stream.read(&mut buffer).unwrap_or(0);
    let request = String::from_utf8_lossy(&buffer[..read]);
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .context("the browser sent an unexpected request")?;
    let url = Url::parse(&format!("http://127.0.0.1{target}"))
        .context("the browser sent an unexpected callback URL")?;
    let parameters = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<std::collections::HashMap<_, _>>();

    let result = classify_callback(&parameters, expected_state);
    let (status, message) = match &result {
        Ok(_) => (
            "200 OK",
            "Login complete. You can close this window and return to the terminal.".to_owned(),
        ),
        Err(error) => ("400 Bad Request", error.to_string()),
    };
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>chuanshanjia</title>\
         <body style=\"font-family:system-ui;padding:3rem\"><p>{message}</p></body>"
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();

    result
}

fn classify_callback(
    parameters: &std::collections::HashMap<String, String>,
    expected_state: &str,
) -> Result<String> {
    if let Some(error) = parameters.get("error") {
        let description = parameters
            .get("error_description")
            .map(String::as_str)
            .unwrap_or("no description");
        anyhow::bail!("the identity provider rejected the login: {error} ({description})");
    }
    let state = parameters
        .get("state")
        .context("the callback is missing the state parameter")?;
    ensure!(
        state == expected_state,
        "the callback state does not match this login attempt"
    );
    parameters
        .get("code")
        .filter(|code| !code.is_empty())
        .cloned()
        .context("the callback is missing the authorization code")
}

fn open_browser(url: &str) -> bool {
    let mut command = if cfg!(target_os = "macos") {
        Command::new("open")
    } else if cfg!(target_os = "windows") {
        let mut command = Command::new("cmd");
        command.args(["/c", "start", ""]);
        command
    } else {
        Command::new("xdg-open")
    };
    command
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials(expires_at: Option<i64>) -> Credentials {
        Credentials {
            issuer: "https://idp.example.com".to_owned(),
            client_id: "cli".to_owned(),
            token_endpoint: "https://idp.example.com/token".to_owned(),
            access_token: "old-access".to_owned(),
            refresh_token: Some("refresh-1".to_owned()),
            expires_at,
            revocation_endpoint: None,
        }
    }

    #[test]
    fn pkce_challenge_matches_rfc_7636_example() {
        // Verifier and expected challenge from RFC 7636 appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn generated_verifier_is_url_safe_and_long_enough() {
        let verifier: String = URL_SAFE_NO_PAD.encode(random::<[u8; 32]>());
        assert_eq!(verifier.len(), 43);
        assert!(
            verifier
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "-_".contains(character))
        );
    }

    #[test]
    fn bearer_token_requires_a_jwt_access_token() {
        let mut stored = credentials(None);
        stored.access_token = "x.y.z".to_owned();
        assert_eq!(bearer_token(&stored).unwrap(), "x.y.z");

        stored.access_token = "opaque-access-token".to_owned();
        assert!(bearer_token(&stored).is_err());
    }

    #[test]
    fn freshness_keeps_a_margin_before_expiry() {
        assert!(is_fresh(&credentials(None), 1000));
        assert!(is_fresh(&credentials(Some(2000)), 1000));
        assert!(!is_fresh(&credentials(Some(1060)), 1000));
        assert!(!is_fresh(&credentials(Some(1000)), 1000));
    }

    #[test]
    fn callback_classification_covers_success_and_failures() {
        let mut parameters = std::collections::HashMap::from([
            ("code".to_owned(), "the-code".to_owned()),
            ("state".to_owned(), "the-state".to_owned()),
        ]);
        assert_eq!(
            classify_callback(&parameters, "the-state").unwrap(),
            "the-code"
        );

        assert!(classify_callback(&parameters, "other-state").is_err());
        parameters.remove("code");
        assert!(classify_callback(&parameters, "the-state").is_err());

        parameters.insert("error".to_owned(), "access_denied".to_owned());
        parameters.insert("error_description".to_owned(), "user said no".to_owned());
        let error = classify_callback(&parameters, "the-state")
            .unwrap_err()
            .to_string();
        assert!(error.contains("access_denied"));
        assert!(error.contains("user said no"));
    }

    #[test]
    fn credentials_round_trip_through_a_private_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = credentials_path(dir.path());
        let credentials = credentials(Some(1234));

        store(&path, &credentials).unwrap();
        let loaded = load(&path).unwrap().unwrap();

        assert_eq!(loaded.issuer, credentials.issuer);
        assert_eq!(loaded.access_token, credentials.access_token);
        assert_eq!(loaded.refresh_token.as_deref(), Some("refresh-1"));
        assert_eq!(loaded.expires_at, Some(1234));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn missing_credentials_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(&credentials_path(dir.path())).unwrap().is_none());
    }

    #[test]
    fn token_exchange_sends_pkce_parameters() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/token")
            .match_header("content-type", mockito::Matcher::Regex("form".into()))
            .match_body(mockito::Matcher::AllOf(vec![
                mockito::Matcher::UrlEncoded("grant_type".into(), "authorization_code".into()),
                mockito::Matcher::UrlEncoded("code".into(), "the-code".into()),
                mockito::Matcher::UrlEncoded("code_verifier".into(), "the-verifier".into()),
                mockito::Matcher::UrlEncoded("client_id".into(), "cli".into()),
                mockito::Matcher::UrlEncoded(
                    "redirect_uri".into(),
                    "http://127.0.0.1:8765/callback".into(),
                ),
            ]))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600}"#,
            )
            .create();

        let tokens = exchange_code(
            &client().unwrap(),
            &format!("{}/token", server.url()),
            "cli",
            "the-code",
            "http://127.0.0.1:8765/callback",
            "the-verifier",
            &[],
        )
        .unwrap();

        mock.assert();
        assert_eq!(tokens.access_token, "access-1");
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh-1"));
        assert_eq!(tokens.expires_in, Some(3600));
    }

    #[test]
    fn refresh_sends_the_refresh_token_and_keeps_it_when_not_rotated() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/token")
            .match_body(mockito::Matcher::AllOf(vec![
                mockito::Matcher::UrlEncoded("grant_type".into(), "refresh_token".into()),
                mockito::Matcher::UrlEncoded("refresh_token".into(), "refresh-1".into()),
                mockito::Matcher::UrlEncoded("client_id".into(), "cli".into()),
            ]))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"access_token":"access-2","expires_in":60}"#)
            .create();

        let mut stored = credentials(None);
        stored.token_endpoint = server.url().to_string() + "/token";
        let refreshed = refresh(&stored).unwrap();

        mock.assert();
        assert_eq!(refreshed.access_token, "access-2");
        assert_eq!(refreshed.refresh_token.as_deref(), Some("refresh-1"));
        assert_eq!(refreshed.issuer, stored.issuer);
        assert!(refreshed.expires_at.unwrap() >= now() + 60);
    }

    #[test]
    fn refresh_reports_oauth_errors() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/token")
            .with_status(400)
            .with_header("content-type", "application/json")
            .with_body(r#"{"error":"invalid_grant","error_description":"refresh token expired"}"#)
            .create();

        let mut stored = credentials(None);
        stored.token_endpoint = server.url().to_string() + "/token";
        let error = refresh(&stored).unwrap_err().to_string();

        mock.assert();
        assert!(error.contains("400"), "{error}");
        assert!(error.contains("invalid_grant"), "{error}");
        assert!(error.contains("refresh token expired"), "{error}");
    }

    #[test]
    fn refresh_without_a_refresh_token_asks_for_a_new_login() {
        let mut stored = credentials(None);
        stored.refresh_token = None;
        let error = refresh(&stored).unwrap_err().to_string();
        assert!(error.contains("login"), "{error}");
    }

    #[test]
    fn credentials_expiry_prefers_expires_in_then_the_jwt_exp_claim() {
        let jwt = |exp: i64| {
            let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"EdDSA","typ":"at+jwt"}"#);
            let payload = URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#).as_bytes());
            format!("{header}.{payload}.signature")
        };
        let build = |access_token: String, expires_in: Option<i64>| {
            credentials_from(
                "https://idp.example.com".to_owned(),
                "cli",
                "https://idp.example.com/token",
                TokenResponse {
                    access_token,
                    refresh_token: None,
                    expires_in,
                },
                None,
            )
        };

        let explicit = build(jwt(1_700_000_000), Some(600));
        let expected = now() + 600;
        assert!(
            (explicit.expires_at.expect("expires_at") - expected).abs() <= 2,
            "expires_in must win over the JWT exp claim"
        );

        // Some IdPs omit `expires_in`; without a fallback the session would be
        // treated as fresh forever and never refreshed.
        let fallback = build(jwt(1_700_000_000), None);
        assert_eq!(fallback.expires_at, Some(1_700_000_000));

        let opaque = build("not-a-jwt".to_owned(), None);
        assert_eq!(opaque.expires_at, None);
    }

    #[test]
    fn jwt_expiry_ignores_malformed_tokens() {
        assert_eq!(jwt_expiry("not-a-jwt"), None);
        assert_eq!(jwt_expiry("header.!invalid.signature"), None);
        assert_eq!(
            jwt_expiry(&format!(
                "header.{}.signature",
                URL_SAFE_NO_PAD.encode(br#"{"sub":"x"}"#)
            )),
            None
        );
        assert_eq!(
            jwt_expiry(&format!(
                "header.{}.signature",
                URL_SAFE_NO_PAD.encode(br#"{"exp":1700000000}"#)
            )),
            Some(1_700_000_000)
        );
    }

    #[test]
    fn discovery_rejects_unusable_issuers() {
        let http = client().unwrap();
        for issuer in ["ftp://idp.example.com", "not a url"] {
            assert!(discover(&http, issuer).is_err(), "accepted {issuer:?}");
        }
    }
}
