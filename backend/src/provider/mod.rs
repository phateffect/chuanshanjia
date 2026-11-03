pub mod es;
pub mod service;
pub mod sls;

use async_trait::async_trait;
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

use crate::search::SearchRequest;

#[async_trait]
pub trait SearchProvider: Send + Sync {
    async fn search(&self, index: &str, request: &SearchRequest) -> Result<Value, ProviderError>;
}

#[derive(Debug)]
pub struct ProviderError {
    status: StatusCode,
    body: Value,
}

impl ProviderError {
    pub fn new(
        status: StatusCode,
        error_type: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            status,
            body: json!({
                "error": {
                    "type": error_type.into(),
                    "reason": reason.into()
                },
                "status": status.as_u16()
            }),
        }
    }

    pub fn upstream(status: StatusCode, body: Value) -> Self {
        Self { status, body }
    }
}

impl IntoResponse for ProviderError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

pub struct UnconfiguredProvider;

#[async_trait]
impl SearchProvider for UnconfiguredProvider {
    async fn search(&self, _index: &str, _request: &SearchRequest) -> Result<Value, ProviderError> {
        Err(ProviderError::new(
            StatusCode::NOT_IMPLEMENTED,
            "provider_not_configured",
            "no search provider is configured",
        ))
    }
}
