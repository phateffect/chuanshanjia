use std::{sync::Arc, time::Duration};

use aliyun_log_rust_sdk::{Client, Error as SlsError, GetLogsResponse};
use async_trait::async_trait;
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::{
    provider::{ProviderError, SearchProvider},
    search::{SearchQuery, SearchRequest},
};

const MAX_LOGS: u32 = 1000;
const NATIVE_PAGE_SIZE: u32 = 100;

pub struct SlsProvider {
    client: Arc<dyn SlsSearchClient>,
    project: String,
}

impl SlsProvider {
    pub fn new(client: Client, project: String) -> Self {
        Self {
            client: Arc::new(client),
            project,
        }
    }
}

#[async_trait]
impl SearchProvider for SlsProvider {
    async fn search(&self, index: &str, request: &SearchRequest) -> Result<Value, ProviderError> {
        let mut query = SlsQuery::new(
            &self.project,
            index,
            request,
            chrono::Utc::now().timestamp(),
        )?;
        let started = std::time::Instant::now();
        let mut logs = Vec::new();
        let mut complete = true;

        loop {
            let response = self.client.get_logs(&query).await.map_err(sls_error)?;
            complete &= response.is_complete();
            let page = response.take_logs();
            let full_page = page.len() == NATIVE_PAGE_SIZE as usize;
            logs.extend(page);

            if !full_page || logs.len() >= MAX_LOGS as usize {
                break;
            }
            query.offset += NATIVE_PAGE_SIZE;
        }

        Ok(search_response(logs, complete, started.elapsed()))
    }
}

#[derive(Clone, Debug)]
struct SlsQuery {
    project: String,
    logstore: String,
    from: i64,
    to: i64,
    query: String,
    offset: u32,
}

impl SlsQuery {
    fn new(
        project: &str,
        logstore: &str,
        request: &SearchRequest,
        now: i64,
    ) -> Result<Self, ProviderError> {
        let range = request.time_range(now)?;
        Ok(Self {
            project: project.to_owned(),
            logstore: logstore.to_owned(),
            from: range.start,
            to: range.end,
            query: sls_query(&request.query)?,
            offset: 0,
        })
    }
}

#[async_trait]
trait SlsSearchClient: Send + Sync {
    async fn get_logs(&self, request: &SlsQuery) -> Result<GetLogsResponse, SlsError>;
}

#[async_trait]
impl SlsSearchClient for Client {
    async fn get_logs(&self, request: &SlsQuery) -> Result<GetLogsResponse, SlsError> {
        Client::get_logs(self, &request.project, &request.logstore)
            .from(request.from)
            .to(request.to)
            .query(request.query.as_str())
            .offset(request.offset)
            .line(NATIVE_PAGE_SIZE)
            .send()
            .await
            .map(|response| response.take_body())
    }
}

fn sls_query(query: &SearchQuery) -> Result<String, ProviderError> {
    let mut parts = vec![
        query
            .keywords
            .as_deref()
            .filter(|value| !value.is_empty())
            .map(quote_query_string)
            .unwrap_or_else(|| "*".to_owned()),
    ];
    for (field, value) in &query.filter {
        parts.push(format!(
            "{}: {}",
            quote_query_string(field),
            native_literal(field, value)?
        ));
    }
    Ok(format!("{} | with_pack_meta", parts.join(" AND ")))
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
            "unsupported_sls_filter",
            format!("filter field {field:?} must have a string, number, or boolean value"),
        )),
    }
}

fn search_response(
    logs: Vec<std::collections::HashMap<String, String>>,
    complete: bool,
    elapsed: Duration,
) -> Value {
    let count = logs.len();
    let took = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);

    json!({
        "took": took,
        "count": count,
        "complete": complete,
        "logs": logs
    })
}

fn sls_error(error: SlsError) -> ProviderError {
    match error {
        SlsError::Server {
            error_code,
            error_message,
            http_status,
            request_id,
        } => {
            let status = u16::try_from(http_status)
                .ok()
                .and_then(|status| StatusCode::from_u16(status).ok())
                .unwrap_or(StatusCode::BAD_GATEWAY);
            let reason = match request_id {
                Some(request_id) => format!("{error_message} (request ID: {request_id})"),
                None => error_message,
            };
            ProviderError::new(status, error_code, reason)
        }
        error => ProviderError::new(
            StatusCode::BAD_GATEWAY,
            "sls_request_failed",
            error.to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex};

    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode, header::CONTENT_TYPE},
        response::Response,
    };
    use tower::ServiceExt;

    use super::*;
    use crate::{app_with_provider, search::DEFAULT_LOOKBACK_SECONDS};

    struct MockSlsClient {
        requests: Mutex<Vec<SlsQuery>>,
        responses: Mutex<VecDeque<Result<GetLogsResponse, SlsError>>>,
    }

    impl MockSlsClient {
        fn success(response: Value) -> Self {
            Self::pages(vec![response])
        }

        fn pages(responses: Vec<Value>) -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
                responses: Mutex::new(
                    responses
                        .into_iter()
                        .map(|response| Ok(serde_json::from_value(response).unwrap()))
                        .collect(),
                ),
            }
        }

        fn failure(error: SlsError) -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
                responses: Mutex::new(VecDeque::from([Err(error)])),
            }
        }
    }

    #[async_trait]
    impl SlsSearchClient for MockSlsClient {
        async fn get_logs(&self, request: &SlsQuery) -> Result<GetLogsResponse, SlsError> {
            self.requests.lock().unwrap().push(request.clone());
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("mock has no response")
        }
    }

    fn app(client: Arc<MockSlsClient>) -> axum::Router {
        app_with_provider(Arc::new(SlsProvider {
            client,
            project: "test-project".to_owned(),
        }))
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

    fn sls_response(progress: &str) -> Value {
        json!({
            "meta": {"progress": progress},
            "data": [{
                "__time__": "1760000000",
                "service": "order",
                "attempt": "2",
                "message": "connection timeout"
            }]
        })
    }

    fn sls_page(progress: &str, count: usize) -> Value {
        json!({
            "meta": {"progress": progress},
            "data": (0..count)
                .map(|position| json!({"position": position.to_string()}))
                .collect::<Vec<_>>()
        })
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
            sls_query(&query).unwrap(),
            r#""connection timeout" AND "attempt": 2 AND "retryable": true AND "service": "order" | with_pack_meta"#
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
            sls_query(&query).unwrap(),
            r#""say \"hello\" \\ path" AND "path\\name": "C:\\logs" AND "request\"method": "O'Reilly" | with_pack_meta"#
        );
    }

    #[test]
    fn filter_only_query_uses_native_field_query() {
        let query: SearchQuery = serde_json::from_value(json!({
            "filter": {"service": "order"},
            "keywords": ""
        }))
        .unwrap();

        assert_eq!(
            sls_query(&query).unwrap(),
            r#"* AND "service": "order" | with_pack_meta"#
        );
    }

    #[test]
    fn empty_query_matches_all_logs() {
        assert_eq!(
            sls_query(&SearchQuery::default()).unwrap(),
            "* | with_pack_meta"
        );
    }

    #[test]
    fn empty_keyword_is_ignored() {
        let query = SearchQuery {
            keywords: Some(String::new()),
            ..SearchQuery::default()
        };
        assert_eq!(sls_query(&query).unwrap(), "* | with_pack_meta");
    }

    #[tokio::test]
    async fn forwards_request_and_normalizes_logs() {
        let client = Arc::new(MockSlsClient::success(sls_response("Complete")));
        let response = app(client.clone())
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

        assert_eq!(response.status(), StatusCode::OK);
        let mut body = body(response).await;
        assert!(body["took"].is_u64());
        body.as_object_mut().unwrap().remove("took");
        assert_eq!(
            body,
            json!({
                "count": 1,
                "complete": true,
                "logs": [{
                    "__time__": "1760000000",
                    "service": "order",
                    "attempt": "2",
                    "message": "connection timeout"
                }]
            })
        );

        let requests = client.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.project, "test-project");
        assert_eq!(request.logstore, "app-logs");
        assert_eq!(request.from, 1760000000);
        assert_eq!(request.to, 1760003600);
        assert_eq!(
            request.query,
            r#""timeout" AND "service": "order" | with_pack_meta"#
        );
        assert_eq!(request.offset, 0);
    }

    #[tokio::test]
    async fn response_preserves_native_field_names_and_values() {
        let client = Arc::new(MockSlsClient::success(json!({
            "meta": {"progress": "Complete"},
            "data": [{
                "traceID": "trace-1",
                "spanID": "span-1",
                "timeUnixNano": "123456789",
                "__pack_meta__": "pack-meta",
                "attribute": "{\"key\":\"value\"}"
            }]
        })));
        let response = app(client)
            .oneshot(request(json!({"query": {}})))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = body(response).await;
        let log = &body["logs"][0];
        assert_eq!(log["traceID"], "trace-1");
        assert_eq!(log["spanID"], "span-1");
        assert_eq!(log["timeUnixNano"], "123456789");
        assert_eq!(log["__pack_meta__"], "pack-meta");
        assert_eq!(log.as_object().unwrap().len(), 5);
        assert!(log.get("traceid").is_none());
        assert!(log.get("kv").is_none());
    }

    #[test]
    fn query_time_defaults_to_one_hour() {
        let request: SearchRequest = serde_json::from_value(json!({"query": {}})).unwrap();
        let query = SlsQuery::new("test-project", "app-logs", &request, 5000).unwrap();

        assert_eq!(query.from, 5000 - DEFAULT_LOOKBACK_SECONDS);
        assert_eq!(query.to, 5000);
    }

    #[tokio::test]
    async fn keyword_search_paginates_and_exposes_incomplete_progress() {
        let client = Arc::new(MockSlsClient::pages(vec![
            sls_page("Complete", NATIVE_PAGE_SIZE as usize),
            sls_page("Incomplete", 1),
        ]));
        let response = app(client.clone())
            .oneshot(request(json!({"query": {"keywords": "timeout"}})))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = body(response).await;
        assert_eq!(body["complete"], false);
        assert_eq!(body["count"], 101);
        assert_eq!(body["logs"].as_array().unwrap().len(), 101);

        let requests = client.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests
                .iter()
                .all(|request| request.query == r#""timeout" | with_pack_meta"#)
        );
        assert_eq!(
            requests
                .iter()
                .map(|request| request.offset)
                .collect::<Vec<_>>(),
            vec![0, NATIVE_PAGE_SIZE]
        );
    }

    #[tokio::test]
    async fn native_search_returns_at_most_one_thousand_logs() {
        let client = Arc::new(MockSlsClient::pages(
            (0..MAX_LOGS / NATIVE_PAGE_SIZE)
                .map(|_| sls_page("Complete", NATIVE_PAGE_SIZE as usize))
                .collect(),
        ));
        let response = app(client.clone())
            .oneshot(request(json!({"query": {"keywords": "timeout"}})))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = body(response).await;
        assert_eq!(body["count"], MAX_LOGS);
        assert_eq!(body["logs"].as_array().unwrap().len(), MAX_LOGS as usize);

        let requests = client.requests.lock().unwrap();
        assert_eq!(requests.len(), (MAX_LOGS / NATIVE_PAGE_SIZE) as usize);
        assert_eq!(requests.last().unwrap().offset, MAX_LOGS - NATIVE_PAGE_SIZE);
    }

    #[tokio::test]
    async fn rejects_non_scalar_filters_without_calling_sls() {
        let client = Arc::new(MockSlsClient::success(sls_response("Complete")));
        let response = app(client.clone())
            .oneshot(request(json!({
                "query": {"filter": {"payload": {"nested": true}}}
            })))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body(response).await["error"]["type"],
            "unsupported_sls_filter"
        );
        assert!(client.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn preserves_sls_server_error_status() {
        let client = Arc::new(MockSlsClient::failure(SlsError::Server {
            error_code: "AccessDenied".to_owned(),
            error_message: "denied".to_owned(),
            http_status: 403,
            request_id: Some("req-1".to_owned()),
        }));
        let response = app(client)
            .oneshot(request(json!({"query": {}})))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            body(response).await,
            json!({
                "error": {
                    "type": "AccessDenied",
                    "reason": "denied (request ID: req-1)"
                },
                "status": 403
            })
        );
    }

    #[tokio::test]
    async fn sdk_client_errors_return_bad_gateway() {
        let client = Arc::new(MockSlsClient::failure(SlsError::InvalidConfig(
            aliyun_log_rust_sdk::ConfigError::InvalidAccessKey,
        )));
        let response = app(client)
            .oneshot(request(json!({"query": {}})))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(body(response).await["error"]["type"], "sls_request_failed");
    }
}
