use async_trait::async_trait;
use axum::http::StatusCode;
use hmac::{Hmac, Mac};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    provider::{ProviderError, SearchProvider},
    search::{SearchQuery, SearchRequest},
};

const MAX_LOGS: usize = 1000;
const NATIVE_PAGE_SIZE: u32 = 100;
const SEARCH_LOGS_PATH: &str = "SearchLogs";
const API_VERSION: &str = "0.3.0";
const TLS_SERVICE: &str = "TLS";
const SORT: &str = "desc";
const X_DATE_FORMAT: &str = "%Y%m%dT%H%M%SZ";

/// Volcano Engine TLS (Torch Log Service) provider. TLS has no official Rust
/// SDK, so this talks to `POST /SearchLogs` directly and signs requests with
/// the Volcano Engine signature (HMAC-SHA256, service name `TLS`).
pub struct TlsProvider {
    client: Client,
    endpoint: Url,
    region: String,
    access_key_id: String,
    access_key_secret: String,
    security_token: Option<String>,
}

impl TlsProvider {
    pub fn new(
        endpoint: Url,
        region: String,
        access_key_id: String,
        access_key_secret: String,
        security_token: Option<String>,
    ) -> Self {
        Self {
            client: Client::new(),
            endpoint,
            region,
            access_key_id,
            access_key_secret,
            security_token,
        }
    }

    async fn search_page(
        &self,
        topic_id: &str,
        query: &str,
        start_time: i64,
        end_time: i64,
        context: Option<&str>,
    ) -> Result<TlsSearchResponse, ProviderError> {
        let request = TlsSearchRequest {
            topic_id,
            query,
            start_time,
            end_time,
            limit: NATIVE_PAGE_SIZE,
            sort: SORT,
            context,
        };
        let body = serde_json::to_vec(&request).map_err(|error| {
            ProviderError::new(
                StatusCode::BAD_GATEWAY,
                "tls_request_failed",
                error.to_string(),
            )
        })?;
        let x_date = chrono::Utc::now().format(X_DATE_FORMAT).to_string();
        let url = search_logs_url(&self.endpoint)?;
        let (authorization, body_hash) = authorization_header(&SigningInput {
            access_key_id: &self.access_key_id,
            access_key_secret: &self.access_key_secret,
            security_token: self.security_token.as_deref(),
            host: &host_header(&self.endpoint),
            x_date: &x_date,
            path: url.path(),
            body: &body,
            region: &self.region,
        });
        let mut builder = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header("X-Date", &x_date)
            .header("X-Content-Sha256", &body_hash)
            .header("X-Tls-Apiversion", API_VERSION)
            .header("Authorization", authorization);
        if let Some(token) = &self.security_token {
            builder = builder.header("X-Security-Token", token);
        }
        let response = builder.body(body).send().await.map_err(|error| {
            ProviderError::new(
                StatusCode::BAD_GATEWAY,
                "tls_request_failed",
                error.without_url().to_string(),
            )
        })?;
        let status = response.status();
        let request_id = response
            .headers()
            .get("x-tls-requestid")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body: Value = response.json().await.map_err(|error| {
            ProviderError::new(
                StatusCode::BAD_GATEWAY,
                "invalid_tls_response",
                error.without_url().to_string(),
            )
        })?;

        if !status.is_success() {
            return Err(upstream_error(status, body, request_id.as_deref()));
        }
        serde_json::from_value(body).map_err(|error| {
            ProviderError::new(
                StatusCode::BAD_GATEWAY,
                "invalid_tls_response",
                error.to_string(),
            )
        })
    }
}

#[async_trait]
impl SearchProvider for TlsProvider {
    async fn search(&self, index: &str, request: &SearchRequest) -> Result<Value, ProviderError> {
        let range = request.time_range(chrono::Utc::now().timestamp())?;
        let query = tls_query(&request.query)?;
        let start_time = range.start.saturating_mul(1000);
        let end_time = range.end.saturating_mul(1000);
        let mut context: Option<String> = None;
        let mut logs = Vec::new();
        let mut complete = true;
        let mut took: Option<i64> = None;

        loop {
            let response = self
                .search_page(index, &query, start_time, end_time, context.as_deref())
                .await?;
            complete &= matches!(response.result_status.as_deref(), Some("complete"));
            if let Some(elapsed) = response.elapsed_millisecond {
                took = Some(took.unwrap_or_default() + elapsed);
            }
            let page = response.logs.unwrap_or_default();
            let full_page = page.len() == NATIVE_PAGE_SIZE as usize;
            logs.extend(page);
            let list_over = response.list_over.unwrap_or(true);
            let next = response.context.filter(|context| !context.is_empty());
            if !full_page || list_over || logs.len() >= MAX_LOGS || next.is_none() {
                complete &= list_over;
                break;
            }
            context = next;
        }

        Ok(search_response(logs, complete, took))
    }
}

#[derive(Serialize)]
struct TlsSearchRequest<'a> {
    #[serde(rename = "TopicId")]
    topic_id: &'a str,
    #[serde(rename = "Query")]
    query: &'a str,
    #[serde(rename = "StartTime")]
    start_time: i64,
    #[serde(rename = "EndTime")]
    end_time: i64,
    #[serde(rename = "Limit")]
    limit: u32,
    #[serde(rename = "Sort")]
    sort: &'a str,
    #[serde(rename = "Context", skip_serializing_if = "Option::is_none")]
    context: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
struct TlsSearchResponse {
    #[serde(rename = "ResultStatus", default)]
    result_status: Option<String>,
    #[serde(rename = "ListOver", default)]
    list_over: Option<bool>,
    #[serde(rename = "Context", default)]
    context: Option<String>,
    #[serde(rename = "Logs", default)]
    logs: Option<Vec<Value>>,
    #[serde(rename = "ElapsedMillisecond", default)]
    elapsed_millisecond: Option<i64>,
}

fn tls_query(query: &SearchQuery) -> Result<String, ProviderError> {
    let mut parts = Vec::new();
    if let Some(keywords) = query.keywords.as_deref().filter(|value| !value.is_empty()) {
        parts.push(quote_query_string(keywords));
    }
    for (field, value) in &query.filter {
        parts.push(format!(
            "{}:{}",
            field_token(field),
            native_literal(field, value)?
        ));
    }
    if parts.is_empty() {
        Ok("*".to_owned())
    } else {
        Ok(parts.join(" AND "))
    }
}

fn field_token(field: &str) -> String {
    let plain = field
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
    if plain {
        field.to_owned()
    } else {
        quote_query_string(field)
    }
}

fn quote_query_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn native_literal(field: &str, value: &Value) -> Result<String, ProviderError> {
    match value {
        Value::String(value) => Ok(quote_query_string(value)),
        Value::Number(value) => Ok(value.to_string()),
        Value::Bool(value) => Ok(value.to_string()),
        Value::Null | Value::Array(_) | Value::Object(_) => Err(ProviderError::new(
            StatusCode::BAD_REQUEST,
            "unsupported_tls_filter",
            format!("filter field {field:?} must have a string, number, or boolean value"),
        )),
    }
}

fn search_response(logs: Vec<Value>, complete: bool, took: Option<i64>) -> Value {
    let count = logs.len();
    let mut response = json!({"count": count, "complete": complete, "logs": logs});
    if let Some(took) = took {
        response["took"] = json!(u64::try_from(took).unwrap_or(u64::MAX));
    }
    response
}

fn upstream_error(status: StatusCode, body: Value, request_id: Option<&str>) -> ProviderError {
    if let (Some(error_code), Some(error_message)) = (
        body.get("ErrorCode").and_then(Value::as_str),
        body.get("ErrorMessage").and_then(Value::as_str),
    ) {
        let reason = match request_id {
            Some(request_id) => format!("{error_message} (request ID: {request_id})"),
            None => error_message.to_owned(),
        };
        ProviderError::new(status, error_code, reason)
    } else {
        ProviderError::upstream(status, body)
    }
}

fn search_logs_url(endpoint: &Url) -> Result<Url, ProviderError> {
    let mut url = endpoint.clone();
    url.path_segments_mut()
        .map_err(|_| {
            ProviderError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "invalid_tls_endpoint",
                "TLS endpoint URL cannot be used as a base URL",
            )
        })?
        .pop_if_empty()
        .push(SEARCH_LOGS_PATH);
    Ok(url)
}

/// The Host header value as it goes on the wire (bracketed IPv6, explicit
/// port), which must match the value that enters the signature.
fn host_header(url: &Url) -> String {
    let Some(host) = url.host_str() else {
        return String::new();
    };
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    }
}

/// Everything that enters a TLS request signature.
struct SigningInput<'a> {
    access_key_id: &'a str,
    access_key_secret: &'a str,
    security_token: Option<&'a str>,
    /// Host header value as it goes on the wire.
    host: &'a str,
    /// UTC timestamp in `YYYYMMDDTHHMMSSZ` form.
    x_date: &'a str,
    path: &'a str,
    body: &'a [u8],
    region: &'a str,
}

/// Computes the `Authorization` header and the `X-Content-Sha256` value for a
/// request, following the Volcano Engine signature algorithm implemented by
/// the official Go and Python SDKs: every sent `X-*`, `Content-Type`,
/// `Content-Md5`, and `Host` header is signed, the canonical request joins
/// method, path, query, canonical headers (each line terminated by `\n`),
/// signed header names, and the hex SHA-256 of the body, and the signing key
/// is an HMAC chain over date, region, service, and `"request"`.
fn authorization_header(input: &SigningInput<'_>) -> (String, String) {
    let SigningInput {
        access_key_id,
        access_key_secret,
        security_token,
        host,
        x_date,
        path,
        body,
        region,
    } = input;
    let body_hash = hex_encode(&Sha256::digest(body));
    let headers = signed_headers(host, x_date, &body_hash, *security_token);
    let canonical_request = canonical_request("POST", path, &headers, &body_hash);
    let short_date = &x_date[..8];
    let credential_scope = format!("{short_date}/{region}/{TLS_SERVICE}/request");
    let string_to_sign = [
        "HMAC-SHA256",
        x_date,
        &credential_scope,
        &hex_encode(&Sha256::digest(canonical_request.as_bytes())),
    ]
    .join("\n");
    let mut key = hmac_sha256(access_key_secret.as_bytes(), short_date);
    key = hmac_sha256(&key, region);
    key = hmac_sha256(&key, TLS_SERVICE);
    key = hmac_sha256(&key, "request");
    let signature = hex_encode(&hmac_sha256(&key, &string_to_sign));
    let signed = headers.keys().cloned().collect::<Vec<_>>().join(";");
    (
        format!(
            "HMAC-SHA256 Credential={access_key_id}/{credential_scope}, SignedHeaders={signed}, Signature={signature}"
        ),
        body_hash,
    )
}

fn signed_headers(
    host: &str,
    x_date: &str,
    body_hash: &str,
    security_token: Option<&str>,
) -> std::collections::BTreeMap<String, String> {
    let mut headers = std::collections::BTreeMap::from([
        ("content-type".to_owned(), "application/json".to_owned()),
        ("host".to_owned(), host.to_owned()),
        ("x-content-sha256".to_owned(), body_hash.to_owned()),
        ("x-date".to_owned(), x_date.to_owned()),
        ("x-tls-apiversion".to_owned(), API_VERSION.to_owned()),
    ]);
    if let Some(token) = security_token {
        headers.insert("x-security-token".to_owned(), token.to_owned());
    }
    headers
}

fn canonical_request(
    method: &str,
    path: &str,
    headers: &std::collections::BTreeMap<String, String>,
    body_hash: &str,
) -> String {
    let canonical_headers = headers
        .iter()
        .map(|(name, value)| format!("{name}:{value}\n"))
        .collect::<String>();
    let signed_headers = headers.keys().cloned().collect::<Vec<_>>().join(";");
    [
        method,
        path,
        "",
        &canonical_headers,
        &signed_headers,
        body_hash,
    ]
    .join("\n")
}

fn hmac_sha256(key: &[u8], message: &str) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts keys of any length");
    mac.update(message.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode, header::CONTENT_TYPE},
        response::Response,
    };
    use mockito::{Matcher, Server};
    use tower::ServiceExt;

    use super::*;
    use crate::app_with_provider;

    fn provider(endpoint: Url) -> Arc<TlsProvider> {
        Arc::new(TlsProvider::new(
            endpoint,
            "test-region".to_owned(),
            "test-ak".to_owned(),
            "test-sk".to_owned(),
            None,
        ))
    }

    fn app(endpoint: Url) -> axum::Router {
        app_with_provider(provider(endpoint))
    }

    fn request(body: Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/app-logs/_search")
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    async fn body(response: Response) -> Value {
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    fn page_body(query: &str, context: Option<&str>) -> Value {
        let mut body = json!({
            "TopicId": "app-logs",
            "Query": query,
            "StartTime": 1760000000000i64,
            "EndTime": 1760003600000i64,
            "Limit": 100,
            "Sort": "desc"
        });
        if let Some(context) = context {
            body["Context"] = json!(context);
        }
        body
    }

    fn tls_response(list_over: bool, context: &str, logs: Vec<Value>) -> Value {
        json!({
            "ResultStatus": "complete",
            "ListOver": list_over,
            "Context": context,
            "Logs": logs,
            "ElapsedMillisecond": 3,
            "RequestId": "req-1"
        })
    }

    fn log_page(count: usize) -> Vec<Value> {
        (0..count)
            .map(|position| json!({"__time__": "1760000000000", "position": position.to_string()}))
            .collect()
    }

    fn authorization_matcher() -> Matcher {
        Matcher::Regex(
            "^HMAC-SHA256 Credential=test-ak/\\d{8}/test-region/TLS/request, \
             SignedHeaders=content-type;host;x-content-sha256;x-date;x-tls-apiversion, \
             Signature=[0-9a-f]{64}$"
                .to_owned(),
        )
    }

    #[test]
    fn query_converts_filters_and_keywords() {
        let query: SearchQuery = serde_json::from_value(json!({
            "filter": {
                "service": "order",
                "attempt": 2,
                "retryable": true
            },
            "keywords": "connection timeout"
        }))
        .unwrap();

        assert_eq!(
            tls_query(&query).unwrap(),
            r#""connection timeout" AND attempt:2 AND retryable:true AND service:"order""#
        );
    }

    #[test]
    fn query_escapes_identifiers_literals_and_keywords() {
        let query: SearchQuery = serde_json::from_value(json!({
            "filter": {
                "path\\name": "C:\\logs",
                "request\"method": "O'Reilly"
            },
            "keywords": "say \"hello\" \\ path"
        }))
        .unwrap();

        assert_eq!(
            tls_query(&query).unwrap(),
            r#""say \"hello\" \\ path" AND "path\\name":"C:\\logs" AND "request\"method":"O'Reilly""#
        );
    }

    #[test]
    fn filter_only_query_uses_native_field_query() {
        let query: SearchQuery = serde_json::from_value(json!({
            "filter": {"service": "order"},
            "keywords": ""
        }))
        .unwrap();

        assert_eq!(tls_query(&query).unwrap(), r#"service:"order""#);
    }

    #[test]
    fn empty_query_matches_all_logs() {
        assert_eq!(tls_query(&SearchQuery::default()).unwrap(), "*");
    }

    #[test]
    fn empty_keyword_is_ignored() {
        let query = SearchQuery {
            keywords: Some(String::new()),
            ..SearchQuery::default()
        };
        assert_eq!(tls_query(&query).unwrap(), "*");
    }

    #[test]
    fn signature_matches_reference_vectors() {
        // Reference values recomputed with an independent implementation of
        // the documented Volcano Engine signature algorithm.
        let (authorization, body_hash) = authorization_header(&SigningInput {
            access_key_id: "AKIDexample",
            access_key_secret: "secret-example",
            security_token: None,
            host: "tls-cn-beijing.volces.com",
            x_date: "20260102T030405Z",
            path: "/SearchLogs",
            body: br#"{"TopicId":"topic-1","Query":"*","StartTime":1760000000000,"EndTime":1760003600000,"Limit":100,"Sort":"desc"}"#,
            region: "cn-beijing",
        });
        assert_eq!(
            body_hash,
            "1f0256ed07294d804a037543118438a1dd0747be867624c6259f1e30d7cabe1b"
        );
        assert_eq!(
            authorization,
            "HMAC-SHA256 Credential=AKIDexample/20260102/cn-beijing/TLS/request, \
             SignedHeaders=content-type;host;x-content-sha256;x-date;x-tls-apiversion, \
             Signature=dddc8ba3ddd81d5e3b39563ec28435c41d3f29c97ca329e716046a5820d7f6a9"
        );

        let (authorization, _) = authorization_header(&SigningInput {
            access_key_id: "AKIDexample",
            access_key_secret: "secret-example",
            security_token: Some("STS-token-1"),
            host: "tls-ap-southeast-1.volces.com",
            x_date: "19991231T235959Z",
            path: "/SearchLogs",
            body: br#"{"TopicId":"t","Query":"error","StartTime":0,"EndTime":1,"Limit":100,"Sort":"desc","Context":"ctx-1"}"#,
            region: "ap-southeast-1",
        });
        assert_eq!(
            authorization,
            "HMAC-SHA256 Credential=AKIDexample/19991231/ap-southeast-1/TLS/request, \
             SignedHeaders=content-type;host;x-content-sha256;x-date;x-security-token;x-tls-apiversion, \
             Signature=33b8248da454702419ba2ad39400d9988409bb12cc1cc4fc9164eeb3395c521d"
        );
    }

    #[test]
    fn host_header_keeps_explicit_port_and_brackets_ipv6() {
        assert_eq!(
            host_header(&Url::parse("https://tls-cn-beijing.volces.com").unwrap()),
            "tls-cn-beijing.volces.com"
        );
        assert_eq!(
            host_header(&Url::parse("http://127.0.0.1:32123").unwrap()),
            "127.0.0.1:32123"
        );
        assert_eq!(
            host_header(&Url::parse("http://[::1]:32123").unwrap()),
            "[::1]:32123"
        );
    }

    #[tokio::test]
    async fn forwards_request_and_normalizes_logs() {
        let mut server = Server::new_async().await;
        let upstream = tls_response(
            true,
            "",
            vec![json!({
                "__time__": "1760000000000",
                "service": "order",
                "attempt": "2",
                "message": "connection timeout"
            })],
        );
        let tls = server
            .mock("POST", "/SearchLogs")
            .match_header(CONTENT_TYPE.as_str(), "application/json")
            .match_header("x-tls-apiversion", API_VERSION)
            .match_header("x-date", Matcher::Regex(r"^\d{8}T\d{6}Z$".to_owned()))
            .match_header("authorization", authorization_matcher())
            .match_body(Matcher::Json(page_body(
                r#""timeout" AND service:"order""#,
                None,
            )))
            .with_status(200)
            .with_header(CONTENT_TYPE.as_str(), "application/json")
            .with_body(upstream.to_string())
            .create_async()
            .await;

        let response = app(Url::parse(&server.url()).unwrap())
            .oneshot(request(json!({
                "query": {
                    "filter": {"service": "order"},
                    "keywords": "timeout"
                },
                "start": 1760000000,
                "end": 1760003600
            })))
            .await
            .unwrap();

        tls.assert_async().await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body(response).await,
            json!({
                "took": 3,
                "count": 1,
                "complete": true,
                "logs": [{
                    "__time__": "1760000000000",
                    "service": "order",
                    "attempt": "2",
                    "message": "connection timeout"
                }]
            })
        );
    }

    #[tokio::test]
    async fn keyword_search_paginates_with_context() {
        let mut server = Server::new_async().await;
        let first = server
            .mock("POST", "/SearchLogs")
            .match_body(Matcher::Json(page_body(r#""timeout""#, None)))
            .with_status(200)
            .with_header(CONTENT_TYPE.as_str(), "application/json")
            .with_body(
                tls_response(false, "cursor-1", log_page(NATIVE_PAGE_SIZE as usize)).to_string(),
            )
            .create_async()
            .await;
        let second = server
            .mock("POST", "/SearchLogs")
            .match_body(Matcher::Json(page_body(r#""timeout""#, Some("cursor-1"))))
            .with_status(200)
            .with_header(CONTENT_TYPE.as_str(), "application/json")
            .with_body(
                json!({
                    "ResultStatus": "complete",
                    "ListOver": true,
                    "Context": "",
                    "Logs": [{"position": "extra"}],
                    "ElapsedMillisecond": 4
                })
                .to_string(),
            )
            .create_async()
            .await;

        let response = app(Url::parse(&server.url()).unwrap())
            .oneshot(request(json!({
                "query": {"keywords": "timeout"},
                "start": 1760000000,
                "end": 1760003600
            })))
            .await
            .unwrap();

        first.assert_async().await;
        second.assert_async().await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = body(response).await;
        assert_eq!(body["took"], 7);
        assert_eq!(body["complete"], true);
        assert_eq!(body["count"], 101);
        assert_eq!(body["logs"].as_array().unwrap().len(), 101);
    }

    #[tokio::test]
    async fn incomplete_pages_report_partial_results() {
        let mut server = Server::new_async().await;
        let tls = server
            .mock("POST", "/SearchLogs")
            .with_status(200)
            .with_header(CONTENT_TYPE.as_str(), "application/json")
            .with_body(
                json!({
                    "ResultStatus": "incomplete",
                    "ListOver": false,
                    "Context": "cursor-1",
                    "Logs": log_page(1),
                    "ElapsedMillisecond": 3
                })
                .to_string(),
            )
            .create_async()
            .await;

        let response = app(Url::parse(&server.url()).unwrap())
            .oneshot(request(json!({"query": {}})))
            .await
            .unwrap();

        tls.assert_async().await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = body(response).await;
        assert_eq!(body["complete"], false);
        assert_eq!(body["count"], 1);
    }

    #[tokio::test]
    async fn native_search_returns_at_most_one_thousand_logs() {
        let mut server = Server::new_async().await;
        for page in 1..=(MAX_LOGS / NATIVE_PAGE_SIZE as usize) {
            let previous = format!("cursor-{}", page - 1);
            let context = if page == 1 {
                None
            } else {
                Some(previous.as_str())
            };
            server
                .mock("POST", "/SearchLogs")
                .match_body(Matcher::Json(page_body("*", context)))
                .with_status(200)
                .with_header(CONTENT_TYPE.as_str(), "application/json")
                .with_body(
                    tls_response(
                        false,
                        &format!("cursor-{page}"),
                        log_page(NATIVE_PAGE_SIZE as usize),
                    )
                    .to_string(),
                )
                .create_async()
                .await;
        }

        let response = app(Url::parse(&server.url()).unwrap())
            .oneshot(request(json!({
                "query": {},
                "start": 1760000000,
                "end": 1760003600
            })))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = body(response).await;
        assert_eq!(body["count"], MAX_LOGS);
        assert_eq!(body["logs"].as_array().unwrap().len(), MAX_LOGS);
        assert_eq!(body["complete"], false);
    }

    #[tokio::test]
    async fn rejects_non_scalar_filters_without_calling_tls() {
        let response = app(Url::parse("http://127.0.0.1:9").unwrap())
            .oneshot(request(json!({
                "query": {"filter": {"payload": {"nested": true}}}
            })))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body(response).await["error"]["type"],
            "unsupported_tls_filter"
        );
    }

    #[tokio::test]
    async fn preserves_tls_server_error_status_and_request_id() {
        let mut server = Server::new_async().await;
        let tls = server
            .mock("POST", "/SearchLogs")
            .with_status(404)
            .with_header(CONTENT_TYPE.as_str(), "application/json")
            .with_header("x-tls-requestid", "req-404")
            .with_body(
                json!({
                    "ErrorCode": "TopicNotExist",
                    "ErrorMessage": "Topic does not exist.",
                    "RequestId": "req-404"
                })
                .to_string(),
            )
            .create_async()
            .await;

        let response = app(Url::parse(&server.url()).unwrap())
            .oneshot(request(json!({"query": {}})))
            .await
            .unwrap();

        tls.assert_async().await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body(response).await,
            json!({
                "error": {
                    "type": "TopicNotExist",
                    "reason": "Topic does not exist. (request ID: req-404)"
                },
                "status": 404
            })
        );
    }

    #[tokio::test]
    async fn malformed_upstream_error_body_is_passed_through() {
        let mut server = Server::new_async().await;
        let upstream = json!({
            "error": {
                "type": "unexpected",
                "reason": "not a tls error shape"
            },
            "status": 400
        });
        let tls = server
            .mock("POST", "/SearchLogs")
            .with_status(400)
            .with_header(CONTENT_TYPE.as_str(), "application/json")
            .with_body(upstream.to_string())
            .create_async()
            .await;

        let response = app(Url::parse(&server.url()).unwrap())
            .oneshot(request(json!({"query": {}})))
            .await
            .unwrap();

        tls.assert_async().await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body(response).await, upstream);
    }

    #[tokio::test]
    async fn malformed_success_body_returns_bad_gateway() {
        let mut server = Server::new_async().await;
        let tls = server
            .mock("POST", "/SearchLogs")
            .with_status(200)
            .with_header(CONTENT_TYPE.as_str(), "application/json")
            .with_body(json!({"Logs": "corrupt"}).to_string())
            .create_async()
            .await;

        let response = app(Url::parse(&server.url()).unwrap())
            .oneshot(request(json!({"query": {}})))
            .await
            .unwrap();

        tls.assert_async().await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            body(response).await["error"]["type"],
            "invalid_tls_response"
        );
    }

    #[tokio::test]
    async fn request_failure_returns_bad_gateway() {
        let provider = TlsProvider::new(
            Url::parse("file:///tmp/tls-not-used").unwrap(),
            "test-region".to_owned(),
            "test-ak".to_owned(),
            "test-sk".to_owned(),
            None,
        );

        let response = app_with_provider(Arc::new(provider))
            .oneshot(request(json!({"query": {}})))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = body(response).await;
        assert_eq!(body["error"]["type"], "tls_request_failed");
        assert_eq!(body["status"], 502);
    }
}
