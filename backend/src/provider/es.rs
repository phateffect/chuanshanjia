use async_trait::async_trait;
use axum::http::StatusCode;
use reqwest::{Client, Url};
use serde_json::{Map, Value, json};

use crate::{
    provider::{ProviderError, SearchProvider},
    search::{SearchQuery, SearchRequest},
};

pub struct EsProvider {
    client: Client,
    base_url: Url,
}

impl EsProvider {
    pub fn new(base_url: Url) -> Self {
        Self {
            client: Client::new(),
            base_url,
        }
    }

    fn search_url(&self, index: &str) -> Result<Url, ProviderError> {
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .map_err(|_| {
                ProviderError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "invalid_es_base_url",
                    "Elasticsearch base URL cannot be used as a base URL",
                )
            })?
            .pop_if_empty()
            .push(index)
            .push("_search");
        Ok(url)
    }
}

#[async_trait]
impl SearchProvider for EsProvider {
    async fn search(&self, index: &str, request: &SearchRequest) -> Result<Value, ProviderError> {
        let range = request.time_range(chrono::Utc::now().timestamp())?;
        let response = self
            .client
            .post(self.search_url(index)?)
            .json(&elasticsearch_query(&request.query, range))
            .send()
            .await
            .map_err(|error| {
                ProviderError::new(
                    StatusCode::BAD_GATEWAY,
                    "es_request_failed",
                    error.without_url().to_string(),
                )
            })?;
        let status = response.status();
        let body = response.json::<Value>().await.map_err(|error| {
            ProviderError::new(
                StatusCode::BAD_GATEWAY,
                "invalid_es_response",
                error.without_url().to_string(),
            )
        })?;

        if status.is_success() {
            Ok(search_response(&body))
        } else {
            Err(ProviderError::upstream(status, body))
        }
    }
}

fn search_response(body: &Value) -> Value {
    let logs = body["hits"]["hits"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|hit| hit.get("_source").unwrap_or(hit).clone())
        .collect::<Vec<_>>();
    let mut response = Map::new();
    if let Some(took) = body.get("took") {
        response.insert("took".to_owned(), took.clone());
    }
    response.insert("count".to_owned(), json!(logs.len()));
    if let Some(timed_out) = body.get("timed_out").and_then(Value::as_bool) {
        response.insert("complete".to_owned(), Value::Bool(!timed_out));
    }
    response.insert("logs".to_owned(), Value::Array(logs));
    Value::Object(response)
}

fn elasticsearch_query(query: &SearchQuery, range: crate::search::TimeRange) -> Value {
    let mut filters = query
        .filter
        .iter()
        .map(|(field, value)| json!({"term": {(field): value}}))
        .collect::<Vec<_>>();
    filters.push(json!({
        "range": {
            "@timestamp": {
                "gte": range.start,
                "lt": range.end,
                "format": "epoch_second"
            }
        }
    }));

    let mut bool_query = Map::new();
    bool_query.insert("filter".to_owned(), Value::Array(filters));
    if let Some(keywords) = &query.keywords {
        bool_query.insert(
            "must".to_owned(),
            json!([{
                "multi_match": {
                    "query": keywords,
                    "fields": ["*"],
                    "fuzziness": "AUTO"
                }
            }]),
        );
    }

    json!({"query": {"bool": Value::Object(bool_query)}})
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode, header::CONTENT_TYPE},
        response::Response,
    };
    use mockito::Matcher;
    use tower::ServiceExt;

    use super::*;
    use crate::app_with_provider;

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

    fn app(server: &mockito::Server) -> axum::Router {
        app_with_provider(Arc::new(EsProvider::new(
            Url::parse(&server.url()).unwrap(),
        )))
    }

    #[test]
    fn query_converts_filters_keywords_and_time_range() {
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
            elasticsearch_query(
                &query,
                crate::search::TimeRange {
                    start: 1760000000,
                    end: 1760003600
                }
            ),
            json!({
                "query": {
                    "bool": {
                        "filter": [
                            {"term": {"attempt": 2}},
                            {"term": {"retryable": true}},
                            {"term": {"service": "order"}},
                            {"range": {"@timestamp": {
                                "gte": 1760000000,
                                "lt": 1760003600,
                                "format": "epoch_second"
                            }}}
                        ],
                        "must": [{
                            "multi_match": {
                                "query": "connection timeout",
                                "fields": ["*"],
                                "fuzziness": "AUTO"
                            }
                        }]
                    }
                }
            })
        );
    }

    #[test]
    fn empty_query_still_filters_by_time() {
        assert_eq!(
            elasticsearch_query(
                &SearchQuery::default(),
                crate::search::TimeRange {
                    start: 1760000000,
                    end: 1760003600
                }
            ),
            json!({
                "query": {
                    "bool": {
                        "filter": [{"range": {"@timestamp": {
                            "gte": 1760000000,
                            "lt": 1760003600,
                            "format": "epoch_second"
                        }}}]
                    }
                }
            })
        );
    }

    #[tokio::test]
    async fn forwards_search_to_es_and_returns_logs() {
        let mut server = mockito::Server::new_async().await;
        let upstream = json!({
            "took": 3,
            "timed_out": false,
            "hits": {
                "total": {"value": 1, "relation": "eq"},
                "max_score": 1.0,
                "hits": [{
                    "_index": "app-logs",
                    "_id": "log-id",
                    "_score": 1.0,
                    "_source": {"message": "connection timeout"}
                }]
            }
        });
        let es = server
            .mock("POST", "/app-logs/_search")
            .match_header(CONTENT_TYPE.as_str(), "application/json")
            .match_body(Matcher::Json(json!({
                "query": {
                    "bool": {
                        "filter": [
                            {"term": {"service": "order"}},
                            {"range": {"@timestamp": {
                                "gte": 1760000000,
                                "lt": 1760003600,
                                "format": "epoch_second"
                            }}}
                        ],
                        "must": [{
                            "multi_match": {
                                "query": "timeout",
                                "fields": ["*"],
                                "fuzziness": "AUTO"
                            }
                        }]
                    }
                }
            })))
            .with_status(200)
            .with_header(CONTENT_TYPE.as_str(), "application/json")
            .with_body(upstream.to_string())
            .create_async()
            .await;

        let response = app(&server)
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

        es.assert_async().await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body(response).await,
            json!({
                "took": 3,
                "count": 1,
                "complete": true,
                "logs": [{"message": "connection timeout"}]
            })
        );
    }

    #[tokio::test]
    async fn omits_missing_optional_fields_and_preserves_hit_without_source() {
        let mut server = mockito::Server::new_async().await;
        let hit = json!({
            "_index": "app-logs",
            "_id": "log-id",
            "_score": 1.0
        });
        let upstream = json!({
            "hits": {
                "total": {"value": 1, "relation": "eq"},
                "max_score": 1.0,
                "hits": [hit.clone()]
            }
        });
        let es = server
            .mock("POST", "/app-logs/_search")
            .with_status(200)
            .with_header(CONTENT_TYPE.as_str(), "application/json")
            .with_body(upstream.to_string())
            .create_async()
            .await;

        let response = app(&server)
            .oneshot(request(json!({"query": {}})))
            .await
            .unwrap();

        es.assert_async().await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body(response).await,
            json!({
                "count": 1,
                "logs": [hit]
            })
        );
    }

    #[tokio::test]
    async fn preserves_es_error_status_and_body() {
        let mut server = mockito::Server::new_async().await;
        let upstream = json!({
            "error": {
                "type": "index_not_found_exception",
                "reason": "no such index [app-logs]"
            },
            "status": 404
        });
        let es = server
            .mock("POST", "/app-logs/_search")
            .match_body(Matcher::Json(json!({
                "query": {
                    "bool": {
                        "filter": [{"range": {"@timestamp": {
                            "gte": 1760000000,
                            "lt": 1760003600,
                            "format": "epoch_second"
                        }}}]
                    }
                }
            })))
            .with_status(404)
            .with_header(CONTENT_TYPE.as_str(), "application/json")
            .with_body(upstream.to_string())
            .create_async()
            .await;

        let response = app(&server)
            .oneshot(request(json!({
                "query": {},
                "start": 1760000000,
                "end": 1760003600
            })))
            .await
            .unwrap();

        es.assert_async().await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(body(response).await, upstream);
    }

    #[tokio::test]
    async fn request_failure_returns_bad_gateway() {
        let provider = EsProvider::new(Url::parse("file:///tmp/es-not-used").unwrap());

        let response = app_with_provider(Arc::new(provider))
            .oneshot(request(json!({"query": {}})))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = body(response).await;
        assert_eq!(body["error"]["type"], "es_request_failed");
        assert_eq!(body["status"], 502);
    }
}
