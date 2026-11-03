use std::{collections::BTreeMap, sync::Arc};

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::Value;

use crate::provider::{ProviderError, SearchProvider};

pub(crate) const DEFAULT_LOOKBACK_SECONDS: i64 = 3600;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchRequest {
    pub query: SearchQuery,
    pub start: Option<i64>,
    pub end: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchQuery {
    #[serde(default)]
    pub filter: BTreeMap<String, Value>,
    pub keywords: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeRange {
    pub start: i64,
    pub end: i64,
}

impl SearchRequest {
    pub fn time_range(&self, now: i64) -> Result<TimeRange, ProviderError> {
        let end = self.end.unwrap_or(now);
        let start = match self.start {
            Some(start) => start,
            None => end.checked_sub(DEFAULT_LOOKBACK_SECONDS).ok_or_else(|| {
                invalid_time_range("end is too small for the default one-hour lookback")
            })?,
        };
        if start >= end {
            return Err(invalid_time_range("start must be earlier than end"));
        }
        Ok(TimeRange { start, end })
    }
}

fn invalid_time_range(reason: impl Into<String>) -> ProviderError {
    ProviderError::new(StatusCode::BAD_REQUEST, "invalid_time_range", reason)
}

pub async fn search(
    State(provider): State<Arc<dyn SearchProvider>>,
    Path(service): Path<String>,
    Json(request): Json<SearchRequest>,
) -> Response {
    provider
        .search(&service, &request)
        .await
        .map(Json)
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app;
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode, header::CONTENT_TYPE},
    };
    use serde_json::json;
    use tower::ServiceExt;

    fn request(index: &str, body: String) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(format!("/{index}/_search"))
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap()
    }

    #[test]
    fn request_preserves_query_and_time_range() {
        let request: SearchRequest = serde_json::from_value(json!({
            "query": {
                "filter": {
                    "service": "order",
                    "attempt": 2,
                    "retryable": true
                },
                "keywords": "connection timeout"
            },
            "start": 1760000000,
            "end": 1760003600
        }))
        .unwrap();

        assert_eq!(request.query.filter["service"], "order");
        assert_eq!(request.query.filter["attempt"], 2);
        assert_eq!(request.query.filter["retryable"], true);
        assert_eq!(
            request.query.keywords.as_deref(),
            Some("connection timeout")
        );
        assert_eq!(
            request.time_range(0).unwrap(),
            TimeRange {
                start: 1760000000,
                end: 1760003600
            }
        );
    }

    #[test]
    fn time_range_defaults_to_one_hour_before_end() {
        let request: SearchRequest = serde_json::from_value(json!({"query": {}})).unwrap();
        assert_eq!(
            request.time_range(5000).unwrap(),
            TimeRange {
                start: 1400,
                end: 5000
            }
        );

        let request: SearchRequest =
            serde_json::from_value(json!({"query": {}, "end": 2000})).unwrap();
        assert_eq!(
            request.time_range(5000).unwrap(),
            TimeRange {
                start: -1600,
                end: 2000
            }
        );
    }

    #[test]
    fn time_range_rejects_empty_or_reversed_ranges() {
        for (start, end) in [(100, 100), (101, 100)] {
            let request: SearchRequest = serde_json::from_value(json!({
                "query": {},
                "start": start,
                "end": end
            }))
            .unwrap();
            assert!(request.time_range(0).is_err());
        }
    }

    #[tokio::test]
    async fn search_reports_that_no_provider_is_configured() {
        let response = app()
            .oneshot(request(
                "app-logs",
                json!({
                    "query": {
                        "filter": {"level": "ERROR"},
                        "keywords": "timeout"
                    }
                })
                .to_string(),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            body,
            json!({
                "error": {
                    "type": "provider_not_configured",
                    "reason": "no search provider is configured"
                },
                "status": 501
            })
        );
    }

    #[tokio::test]
    async fn search_accepts_an_empty_query() {
        let response = app()
            .oneshot(request("app-logs", json!({"query": {}}).to_string()))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn search_rejects_malformed_json() {
        let response = app()
            .oneshot(request("app-logs", r#"{"query":"#.to_owned()))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn search_rejects_unknown_query_fields() {
        let response = app()
            .oneshot(request(
                "app-logs",
                json!({"query": {"unknown": true}}).to_string(),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
}
