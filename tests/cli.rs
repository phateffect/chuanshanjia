use std::{fs, net::TcpListener, sync::Arc, thread};

use assert_cmd::Command;
use chuanshanjia_backend::{
    app_with_provider, config::AppConfig, provider::service::ServiceProvider,
};
use mockito::{Matcher, Server};
use serde_json::json;
use tempfile::TempDir;

fn chuanshanjia(home: &TempDir) -> Command {
    let mut command = Command::cargo_bin("chuanshanjia").expect("chuanshanjia binary should exist");
    command
        .env("HOME", home.path())
        .env("USERPROFILE", home.path());
    command
}

/// A raw `std::process::Command` for interactive tests that need piped stdout.
/// Port 0 lets the CLI pick a free port and report it in the redirect URI, which
/// keeps parallel tests from racing for a fixed one.
const AUTO_PORT_REDIRECT: &str = "http://127.0.0.1:0/callback";

fn login_command(home: &TempDir) -> std::process::Command {
    let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin!("chuanshanjia"));
    command
        .env("HOME", home.path())
        .env("USERPROFILE", home.path());
    command
}

fn search_cmd(home: &TempDir) -> Command {
    let mut command = chuanshanjia(home);
    command.args([
        "search",
        "foo",
        "--keywords",
        "timeout",
        "--start",
        "1",
        "--end",
        "2",
    ]);
    command
}

fn search(home: &TempDir) -> Command {
    let mut command = chuanshanjia(home);
    command.args([
        "search",
        "foo",
        "--keywords",
        "timeout",
        "--start",
        "1",
        "--end",
        "2",
    ]);
    command
}

fn write_config(home: &TempDir, contents: &str) {
    let config_dir = home.path().join(".config").join("chuanshanjia");
    fs::create_dir_all(&config_dir).expect("config directory should be created");
    fs::write(config_dir.join("config.toml"), contents).expect("config should be written");
}

fn spawn_backend(es_url: &str) -> String {
    let config: AppConfig = toml::from_str(&format!(
        r#"
        [server]
        oidc_issuer = "https://idp.example.com"
        oidc_audiences = ["test-audience"]

        [storages.es]
        provider = "elasticsearch"
        base_url = "{es_url}"

        [services.foo]
        storage = "es"
        index = "app-logs"
        keyword = "preset-keyword"

        [services.foo.filters]
        service = "order"
        "#
    ))
    .expect("backend config should be valid");
    let provider = ServiceProvider::from_config(config).expect("provider should be configured");
    let app = app_with_provider(Arc::new(provider));
    let listener = TcpListener::bind("127.0.0.1:0").expect("backend port should be available");
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();

    drop(thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(listener, app).await.unwrap();
        });
    }));

    format!("http://{address}")
}

#[cfg(unix)]
fn successful_editor(home: &TempDir) -> String {
    use std::os::unix::fs::PermissionsExt;

    let path = home.path().join("editor.sh");
    fs::write(
        &path,
        "#!/bin/sh\ntest \"$1\" = \"$HOME/.config/chuanshanjia/config.toml\"\n",
    )
    .expect("editor script should be written");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
        .expect("editor script should be executable");
    path.display().to_string()
}

#[cfg(windows)]
fn successful_editor(_home: &TempDir) -> String {
    "cmd /c rem".to_string()
}

#[test]
fn config_creates_default_and_opens_editor() {
    let home = TempDir::new().expect("temporary home should be created");
    let editor = successful_editor(&home);

    chuanshanjia(&home)
        .arg("config")
        .env("EDITOR", editor)
        .assert()
        .success();

    let contents = fs::read_to_string(home.path().join(".config/chuanshanjia/config.toml"))
        .expect("default config should exist");
    assert_eq!(
        contents,
        "base_url = \"\"\n# token = \"\"\n# issuer = \"https://oidc.example.com\"\n# client_id = \"chuanshanjia\"\nresource = \"\"\n# scope = \"openid profile email offline_access logs:read\"\n"
    );
}

#[test]
fn config_preserves_existing_file() {
    let home = TempDir::new().expect("temporary home should be created");
    let editor = successful_editor(&home);
    let contents = "base_url = \"http://127.0.0.1:1234/endpoint\"\n";
    write_config(&home, contents);

    chuanshanjia(&home)
        .arg("config")
        .env("EDITOR", editor)
        .assert()
        .success();

    let actual = fs::read_to_string(home.path().join(".config/chuanshanjia/config.toml"))
        .expect("config should exist");
    assert_eq!(actual, contents);
}

#[test]
fn config_requires_editor() {
    let home = TempDir::new().expect("temporary home should be created");

    let assert = chuanshanjia(&home)
        .arg("config")
        .env_remove("EDITOR")
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("EDITOR is not set"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn search_sends_filters_keywords_and_time_range() {
    let start = chrono::DateTime::parse_from_rfc3339("2026-10-14T02:01:02Z")
        .unwrap()
        .timestamp();
    let end = 1792994561;
    let upstream = json!({
        "hits": {"hits": [{"_source": {"message": "found"}}]}
    });
    let mut server = Server::new();
    let request = server
        .mock("POST", "/foo/_search")
        .match_header("content-type", "application/json")
        .match_body(Matcher::Json(json!({
            "query": {
                "filter": {"filterA": "aValue", "filterB": "bValue"},
                "keywords": "someKeyword"
            },
            "start": start,
            "end": end
        })))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(upstream.to_string())
        .expect(1)
        .create();
    let home = TempDir::new().expect("temporary home should be created");
    write_config(&home, &format!("base_url = \"{}\"\n", server.url()));

    let assert = chuanshanjia(&home)
        .args([
            "search",
            "foo",
            "-f",
            "filterA=aValue",
            "-f",
            "filterB=bValue",
            "-k",
            "someKeyword",
            "--start",
            "2026-10-14 10:01:02",
            "--end",
            "1792994561",
        ])
        .assert()
        .success();

    request.assert();
    let output: serde_json::Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(output, upstream);
}

#[test]
fn search_sends_config_token_and_cli_override() {
    let mut server = Server::new();
    let config_request = server
        .mock("POST", "/foo/_search")
        .match_header("authorization", "Bearer config-token")
        .with_status(200)
        .with_body("{}")
        .expect(1)
        .create();
    let override_request = server
        .mock("POST", "/foo/_search")
        .match_header("authorization", "Bearer cli-token")
        .with_status(200)
        .with_body("{}")
        .expect(1)
        .create();
    let home = TempDir::new().expect("temporary home should be created");
    write_config(
        &home,
        &format!(
            "base_url = \"{}\"\ntoken = \"config-token\"\n",
            server.url()
        ),
    );

    chuanshanjia(&home)
        .args([
            "search", "foo", "-k", "timeout", "--start", "1", "--end", "2",
        ])
        .assert()
        .success();
    chuanshanjia(&home)
        .args([
            "search",
            "foo",
            "-k",
            "timeout",
            "--start",
            "1",
            "--end",
            "2",
            "--token",
            "cli-token",
        ])
        .assert()
        .success();

    config_request.assert();
    override_request.assert();
}

#[test]
fn search_rejects_server_errors() {
    let mut server = Server::new();
    let request = server
        .mock("POST", "/foo/_search")
        .with_status(404)
        .with_header("content-type", "application/json")
        .with_body(json!({"error": {"reason": "missing service"}}).to_string())
        .expect(1)
        .create();
    let home = TempDir::new().expect("temporary home should be created");
    write_config(&home, &format!("base_url = \"{}\"\n", server.url()));

    let assert = search(&home).assert().failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(stderr.contains("404"), "unexpected stderr: {stderr}");
    assert!(
        stderr.contains("missing service"),
        "unexpected stderr: {stderr}"
    );
    request.assert();
}

#[test]
fn search_requires_service() {
    let home = TempDir::new().expect("temporary home should be created");

    let assert = chuanshanjia(&home)
        .args(["search", "-k", "timeout"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(stderr.contains("SERVICE"), "unexpected stderr: {stderr}");
}

#[test]
fn search_requires_filter_or_keywords() {
    let home = TempDir::new().expect("temporary home should be created");

    let assert = chuanshanjia(&home)
        .args(["search", "foo"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(stderr.contains("--filter"), "unexpected stderr: {stderr}");
    assert!(stderr.contains("--keywords"), "unexpected stderr: {stderr}");
}

#[test]
fn search_rejects_repeated_keywords() {
    let home = TempDir::new().expect("temporary home should be created");

    let assert = chuanshanjia(&home)
        .args(["search", "foo", "-k", "one", "-k", "two"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("cannot be used multiple times"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn search_rejects_invalid_filters() {
    let home = TempDir::new().expect("temporary home should be created");

    for filter in ["missing-equals", "=empty-key"] {
        let assert = chuanshanjia(&home)
            .args(["search", "foo", "-f", filter])
            .assert()
            .failure();
        let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
        assert!(stderr.contains("KEY=VALUE"), "unexpected stderr: {stderr}");
    }
}

#[test]
fn search_rejects_invalid_or_reversed_time() {
    let home = TempDir::new().expect("temporary home should be created");

    let assert = chuanshanjia(&home)
        .args(["search", "foo", "-k", "timeout", "--start", "invalid"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("invalid timestamp"),
        "unexpected stderr: {stderr}"
    );

    let assert = chuanshanjia(&home)
        .args([
            "search", "foo", "-k", "timeout", "--start", "2", "--end", "1",
        ])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("start must be earlier than end"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn cli_cannot_override_backend_preset_query() {
    let start = 1760000000;
    let end = 1760003600;
    let upstream = json!({
        "took": 3,
        "timed_out": false,
        "hits": {
            "total": {"value": 1, "relation": "eq"},
            "hits": [{"_source": {"message": "from backend"}}]
        }
    });
    let mut es_server = Server::new();
    let es_request = es_server
        .mock("POST", "/app-logs/_search")
        .match_header("content-type", "application/json")
        .match_body(Matcher::Json(json!({
            "query": {
                "bool": {
                    "filter": [
                        {"term": {"service": "order"}},
                        {"range": {"@timestamp": {
                            "gte": start,
                            "lt": end,
                            "format": "epoch_second"
                        }}}
                    ],
                    "must": [{
                        "multi_match": {
                            "query": "preset-keyword",
                            "fields": ["*"],
                            "fuzziness": "AUTO"
                        }
                    }]
                }
            }
        })))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(upstream.to_string())
        .expect(1)
        .create();
    let backend_url = spawn_backend(&es_server.url());
    let home = TempDir::new().expect("temporary home should be created");
    write_config(&home, &format!("base_url = \"{backend_url}\"\n"));

    let assert = chuanshanjia(&home)
        .args([
            "search",
            "foo",
            "-f",
            "service=checkout",
            "-k",
            "request-keyword",
            "--start",
            "1760000000",
            "--end",
            "1760003600",
        ])
        .assert()
        .success();

    es_request.assert();
    let output: serde_json::Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(
        output,
        json!({
            "took": 3,
            "count": 1,
            "complete": true,
            "logs": [{"message": "from backend"}]
        })
    );
}

#[test]
fn missing_config_fails_with_a_clear_error() {
    let home = TempDir::new().expect("temporary home should be created");

    let assert = search(&home).assert().failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("failed to read config file"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn invalid_config_fails_with_a_clear_error() {
    let home = TempDir::new().expect("temporary home should be created");
    write_config(&home, "base_url = 42\n");

    let assert = search(&home).assert().failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("failed to parse config file"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn invalid_base_url_fails_with_a_clear_error() {
    let home = TempDir::new().expect("temporary home should be created");
    write_config(&home, "base_url = \"not a URL\"\n");

    let assert = search(&home).assert().failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("invalid base_url"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn request_failure_fails_with_a_clear_error() {
    let home = TempDir::new().expect("temporary home should be created");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0; 1024];
        let _ = std::io::Read::read(&mut stream, &mut request);
    });
    write_config(&home, &format!("base_url = \"http://{address}\"\n"));

    let assert = search(&home).assert().failure();
    server.join().unwrap();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("failed to POST to"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn help_and_version_do_not_need_config() {
    let home = TempDir::new().expect("temporary home should be created");

    let help = chuanshanjia(&home).arg("--help").assert().success();
    let stdout = String::from_utf8_lossy(&help.get_output().stdout);
    assert!(stdout.contains("Usage:"), "unexpected stdout: {stdout}");

    let search_help = chuanshanjia(&home)
        .args(["search", "--help"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&search_help.get_output().stdout);
    assert!(stdout.contains("SERVICE"), "unexpected stdout: {stdout}");
    assert!(stdout.contains("--start"), "unexpected stdout: {stdout}");
    assert!(stdout.contains("--end"), "unexpected stdout: {stdout}");

    let version = chuanshanjia(&home).arg("--version").assert().success();
    let stdout = String::from_utf8_lossy(&version.get_output().stdout);
    assert_eq!(stdout.trim(), "chuanshanjia 0.1.0");
}

/// Plays the browser: waits for the CLI to listen, then delivers a successful callback.
fn answer_login(port: u16, query: &str) {
    let response = deliver_callback(port, query);
    assert!(
        response.contains("200 OK"),
        "unexpected response: {response}"
    );
}

/// Same, but expects the CLI to reject the callback.
fn answer_login_rejecting(port: u16, query: &str) {
    let response = deliver_callback(port, query);
    assert!(
        response.contains("400 Bad Request"),
        "unexpected response: {response}"
    );
}

fn deliver_callback(port: u16, query: &str) -> String {
    use std::io::{Read, Write};

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        match std::net::TcpStream::connect(("127.0.0.1", port)) {
            Ok(mut stream) => {
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(15)));
                let _ = stream.write_all(
                    format!("GET /callback?{query} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes(),
                );
                let mut response = String::new();
                let _ = stream.read_to_string(&mut response);
                return response;
            }
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(error) => panic!("the CLI never listened on port {port}: {error}"),
        }
    }
}

/// Spawn `login`, then return the process plus the authorization URL it printed.
/// Reading happens on a helper thread so a failure shows the CLI's stderr instead
/// of hanging the test.
fn spawn_login(home: &TempDir, args: &[&str]) -> (std::process::Child, reqwest::Url) {
    use std::io::{BufRead, BufReader};

    let stderr_path = home.path().join("login.stderr");
    let mut child = login_command(home)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::from(
            fs::File::create(&stderr_path).expect("stderr file should be created"),
        ))
        .spawn()
        .expect("login should start");

    let stdout = child.stdout.take().expect("stdout should be piped");
    let (sender, receiver) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let found = BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
            .find(|line| line.starts_with("http://") || line.starts_with("https://"));
        let _ = sender.send(found);
    });

    match receiver.recv_timeout(std::time::Duration::from_secs(20)) {
        Ok(Some(url)) => (child, reqwest::Url::parse(&url).expect("authorization URL")),
        Ok(None) => {
            let _ = child.kill();
            let stderr = fs::read_to_string(&stderr_path).unwrap_or_default();
            panic!("the CLI exited without printing an authorization URL; stderr: {stderr}");
        }
        Err(error) => {
            let _ = child.kill();
            panic!("the CLI printed no authorization URL within 20s: {error}");
        }
    }
}

fn query_parameters(url: &reqwest::Url) -> std::collections::HashMap<String, String> {
    url.query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}

#[test]
fn login_runs_pkce_flow_and_stores_tokens() {
    let resource = "https://logs.example.com";
    let mut idp = Server::new();
    let discovery = idp
        .mock("GET", "/.well-known/openid-configuration")
        .with_body(
            json!({
                "issuer": idp.url(),
                "authorization_endpoint": format!("{}/auth", idp.url()),
                "token_endpoint": format!("{}/token", idp.url()),
            })
            .to_string(),
        )
        .create();
    let token = idp
        .mock("POST", "/token")
        .match_body(Matcher::AllOf(vec![
            Matcher::UrlEncoded("grant_type".into(), "authorization_code".into()),
            Matcher::UrlEncoded("code".into(), "the-code".into()),
            Matcher::UrlEncoded("client_id".into(), "cli".into()),
            Matcher::UrlEncoded("resource".into(), resource.into()),
        ]))
        .with_body(
            json!({
                "access_token": "access-token",
                "refresh_token": "refresh-token",
                "expires_in": 3600
            })
            .to_string(),
        )
        .create();
    let mut backend = Server::new();
    let metadata = backend
        .mock("GET", "/.well-known/oauth-protected-resource")
        .with_body(
            json!({
                "resource": resource,
                "authorization_servers": [idp.url()],
                "scopes_supported": [],
                "bearer_methods_supported": ["header"],
            })
            .to_string(),
        )
        .create();

    let home = TempDir::new().expect("temporary home should be created");
    write_config(
        &home,
        &format!(
            "base_url = \"{}\"\nissuer = \"{}\"\nclient_id = \"cli\"\nresource = \"{resource}\"\n",
            backend.url(),
            idp.url()
        ),
    );
    let (mut child, authorize) = spawn_login(
        &home,
        &[
            "login",
            "--no-browser",
            "--redirect-uri",
            AUTO_PORT_REDIRECT,
            "--scope",
            "openid offline_access",
        ],
    );

    let parameters = query_parameters(&authorize);
    assert_eq!(parameters["response_type"], "code");
    assert_eq!(parameters["client_id"], "cli");
    assert_eq!(parameters["code_challenge_method"], "S256");
    assert!(!parameters["code_challenge"].is_empty());
    assert_eq!(parameters["scope"], "openid offline_access");
    assert_eq!(parameters["resource"], resource);
    let state = parameters["state"].clone();
    assert!(!state.is_empty());
    let redirect = reqwest::Url::parse(&parameters["redirect_uri"]).unwrap();
    assert_eq!(redirect.host_str(), Some("127.0.0.1"));
    assert_eq!(redirect.path(), "/callback");
    let port = redirect
        .port()
        .expect("the redirect URI must carry the resolved port");

    answer_login(port, &format!("code=the-code&state={state}"));

    let status = child.wait().expect("login should finish");
    assert!(status.success(), "login failed with {status}");

    metadata.assert();
    discovery.assert();
    token.assert();
    let credentials = fs::read_to_string(home.path().join(".config/chuanshanjia/credentials.toml"))
        .expect("credentials should be written");
    assert!(credentials.contains("access-token"), "{credentials}");
    assert!(credentials.contains("refresh-token"), "{credentials}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = home.path().join(".config/chuanshanjia/credentials.toml");
        let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[test]
fn login_rejects_a_mismatched_state() {
    let resource = "https://logs.example.com";
    let mut idp = Server::new();
    idp.mock("GET", "/.well-known/openid-configuration")
        .with_body(
            json!({
                "issuer": idp.url(),
                "authorization_endpoint": format!("{}/auth", idp.url()),
                "token_endpoint": format!("{}/token", idp.url()),
            })
            .to_string(),
        )
        .create();
    let mut backend = Server::new();
    backend
        .mock("GET", "/.well-known/oauth-protected-resource")
        .with_body(
            json!({
                "resource": resource,
                "authorization_servers": [idp.url()],
                "scopes_supported": ["logs:read"],
            })
            .to_string(),
        )
        .create();

    let home = TempDir::new().expect("temporary home should be created");
    write_config(
        &home,
        &format!(
            "base_url = \"{}\"\nissuer = \"{}\"\nclient_id = \"cli\"\nresource = \"{resource}\"\n",
            backend.url(),
            idp.url()
        ),
    );
    let (child, authorize) = spawn_login(
        &home,
        &[
            "login",
            "--no-browser",
            "--redirect-uri",
            AUTO_PORT_REDIRECT,
        ],
    );

    let redirect = reqwest::Url::parse(&query_parameters(&authorize)["redirect_uri"]).unwrap();
    let port = redirect
        .port()
        .expect("the redirect URI must carry the port");

    answer_login_rejecting(port, "code=the-code&state=forged");
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(
        !home
            .path()
            .join(".config/chuanshanjia/credentials.toml")
            .exists(),
        "a failed login must not store credentials"
    );
}

#[test]
fn search_refreshes_an_expired_session() {
    let mut idp = Server::new();
    let refresh = idp
        .mock("POST", "/token")
        .match_body(Matcher::AllOf(vec![
            Matcher::UrlEncoded("grant_type".into(), "refresh_token".into()),
            Matcher::UrlEncoded("refresh_token".into(), "refresh-1".into()),
            Matcher::UrlEncoded("client_id".into(), "cli".into()),
        ]))
        .with_body(json!({"access_token": "x.y.z", "expires_in": 3600}).to_string())
        .create();
    let mut backend = Server::new();
    let search = backend
        .mock("POST", "/foo/_search")
        .match_header("authorization", "Bearer x.y.z")
        .with_status(200)
        .with_body("{}")
        .create();

    let home = TempDir::new().expect("temporary home should be created");
    write_config(&home, &format!("base_url = \"{}\"\n", backend.url()));
    let expired = chrono::Utc::now().timestamp() - 10;
    fs::write(
        home.path().join(".config/chuanshanjia/credentials.toml"),
        format!(
            "issuer = \"https://idp.example.com\"\nclient_id = \"cli\"\ntoken_endpoint = \"{}/token\"\naccess_token = \"access-1\"\nrefresh_token = \"refresh-1\"\nexpires_at = {expired}\n",
            idp.url()
        ),
    )
    .unwrap();

    search_cmd(&home).assert().success();

    refresh.assert();
    search.assert();
    let credentials =
        fs::read_to_string(home.path().join(".config/chuanshanjia/credentials.toml")).unwrap();
    assert!(credentials.contains("x.y.z"), "{credentials}");
    assert!(credentials.contains("refresh-1"), "{credentials}");
}

#[test]
fn search_reports_a_session_that_cannot_be_refreshed() {
    let mut idp = Server::new();
    idp.mock("POST", "/token")
        .with_status(400)
        .with_body(json!({"error": "invalid_grant"}).to_string())
        .create();

    let home = TempDir::new().expect("temporary home should be created");
    write_config(&home, "base_url = \"http://127.0.0.1:1\"\n");
    fs::write(
        home.path().join(".config/chuanshanjia/credentials.toml"),
        format!(
            "issuer = \"https://idp.example.com\"\nclient_id = \"cli\"\ntoken_endpoint = \"{}/token\"\naccess_token = \"access-1\"\nrefresh_token = \"refresh-1\"\nexpires_at = 1\n",
            idp.url()
        ),
    )
    .unwrap();

    let assert = search_cmd(&home).assert().failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(stderr.contains("login"), "unexpected stderr: {stderr}");
}

fn write_credentials(home: &TempDir, idp_url: &str) -> std::path::PathBuf {
    let path = home.path().join(".config/chuanshanjia/credentials.toml");
    fs::write(
        &path,
        format!(
            "issuer = \"https://idp.example.com\"\nclient_id = \"cli\"\n\
             token_endpoint = \"{idp_url}/token\"\nrevocation_endpoint = \"{idp_url}/revoke\"\n\
             access_token = \"access-1\"\nrefresh_token = \"refresh-1\"\n"
        ),
    )
    .unwrap();
    path
}

#[test]
fn logout_revokes_the_refresh_token_and_removes_credentials() {
    let mut idp = Server::new();
    let revoked = idp
        .mock("POST", "/revoke")
        .match_body(Matcher::AllOf(vec![
            Matcher::UrlEncoded("token".into(), "refresh-1".into()),
            Matcher::UrlEncoded("token_type_hint".into(), "refresh_token".into()),
            Matcher::UrlEncoded("client_id".into(), "cli".into()),
        ]))
        .with_status(200)
        .create();

    let home = TempDir::new().expect("temporary home should be created");
    write_config(&home, "base_url = \"http://127.0.0.1:1\"\n");
    let path = write_credentials(&home, &idp.url());

    chuanshanjia(&home).arg("logout").assert().success();

    revoked.assert();
    assert!(!path.exists(), "credentials should be removed");
}

#[test]
fn logout_still_removes_credentials_when_revocation_fails() {
    let mut idp = Server::new();
    idp.mock("POST", "/revoke").with_status(500).create();

    let home = TempDir::new().expect("temporary home should be created");
    write_config(&home, "base_url = \"http://127.0.0.1:1\"\n");
    let path = write_credentials(&home, &idp.url());

    let assert = chuanshanjia(&home).arg("logout").assert().success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    // The warning goes to stdout with the rest of the CLI's messages.
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(
        stdout.contains("could not revoke") || stderr.contains("could not revoke"),
        "expected a revocation warning, got: {stdout}{stderr}"
    );
    assert!(!path.exists(), "credentials should still be removed");
}

#[test]
fn logout_local_skips_revocation() {
    let mut idp = Server::new();
    let revoked = idp
        .mock("POST", "/revoke")
        .expect(0)
        .with_status(200)
        .create();

    let home = TempDir::new().expect("temporary home should be created");
    write_config(&home, "base_url = \"http://127.0.0.1:1\"\n");
    let path = write_credentials(&home, &idp.url());

    chuanshanjia(&home)
        .args(["logout", "--local"])
        .assert()
        .success();

    revoked.assert();
    assert!(!path.exists(), "credentials should be removed");
}

#[test]
fn logout_without_credentials_is_a_no_op() {
    let home = TempDir::new().expect("temporary home should be created");
    write_config(&home, "base_url = \"http://127.0.0.1:1\"\n");

    chuanshanjia(&home).arg("logout").assert().success();
}

#[test]
fn login_requires_config_or_flags() {
    let home = TempDir::new().expect("temporary home should be created");
    write_config(&home, "base_url = \"\"\n");

    let assert = chuanshanjia(&home).arg("login").assert().failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(stderr.contains("issuer"), "unexpected stderr: {stderr}");
}

#[test]
fn login_requires_an_explicit_resource() {
    let home = TempDir::new().expect("temporary home should be created");
    write_config(
        &home,
        "base_url = \"http://127.0.0.1:1\"\nissuer = \"https://idp.example.com\"\nclient_id = \"cli\"\n",
    );

    let assert = chuanshanjia(&home).arg("login").assert().failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("set `resource` in the config file or pass --resource"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn login_rejects_a_resource_mismatch_before_oidc_discovery() {
    let mut backend = Server::new();
    let metadata = backend
        .mock("GET", "/.well-known/oauth-protected-resource")
        .with_body(
            json!({
                "resource": "https://logs.example.com",
                "authorization_servers": ["https://idp.example.com"],
                "scopes_supported": ["logs:read"],
            })
            .to_string(),
        )
        .create();
    let home = TempDir::new().expect("temporary home should be created");
    write_config(
        &home,
        &format!(
            "base_url = \"{}\"\nissuer = \"https://idp.example.com\"\nclient_id = \"cli\"\nresource = \"https://wrong.example.com\"\n",
            backend.url()
        ),
    );

    let assert = chuanshanjia(&home).arg("login").assert().failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(
        stderr.contains("does not match backend resource"),
        "unexpected stderr: {stderr}"
    );
    metadata.assert();
}
