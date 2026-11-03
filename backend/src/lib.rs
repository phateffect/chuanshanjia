pub mod auth;
pub mod config;
pub mod provider;
pub mod search;

use std::sync::Arc;

use auth::JwtAuthenticator;
use axum::{
    Router, middleware,
    routing::{get, post},
};
use provider::{SearchProvider, UnconfiguredProvider};
use serde_json::{Value, json};

/// RFC 9728 metadata telling clients which resource this backend serves, which
/// authorization server issues its tokens, and which scopes it requires.
#[derive(Clone, Debug)]
pub struct ResourceMetadata {
    pub resource: String,
    pub authorization_servers: Vec<String>,
    pub scopes_supported: Vec<String>,
}

impl ResourceMetadata {
    fn to_json(&self) -> Value {
        json!({
            "resource": self.resource,
            "authorization_servers": self.authorization_servers,
            "scopes_supported": self.scopes_supported,
            "bearer_methods_supported": ["header"],
        })
    }
}

pub fn app() -> Router {
    app_with_provider(Arc::new(UnconfiguredProvider))
}

pub fn app_with_provider(provider: Arc<dyn SearchProvider>) -> Router {
    app_with_provider_and_auth(provider, None)
}

pub fn app_with_provider_and_auth(
    provider: Arc<dyn SearchProvider>,
    authenticator: Option<Arc<JwtAuthenticator>>,
) -> Router {
    app_full(provider, authenticator, None)
}

pub fn app_full(
    provider: Arc<dyn SearchProvider>,
    authenticator: Option<Arc<JwtAuthenticator>>,
    metadata: Option<ResourceMetadata>,
) -> Router {
    let search = post(search::search);
    let search = match authenticator {
        Some(authenticator) => search.layer(middleware::from_fn_with_state(
            authenticator,
            auth::require_jwt,
        )),
        None => search,
    };
    let router = Router::new()
        .route("/", get(|| async { "hello world" }))
        .route("/{service}/_search", search);
    match metadata {
        Some(metadata) => router.route(
            "/.well-known/oauth-protected-resource",
            get(move || {
                let metadata = metadata.clone();
                async move { axum::Json(metadata.to_json()) }
            }),
        ),
        None => router,
    }
    .with_state(provider)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;

    #[tokio::test]
    async fn root_returns_hello_world() {
        let response = app()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.as_ref(), b"hello world");
    }

    fn search_request(token: Option<&str>) -> Request<Body> {
        let builder = Request::builder()
            .method("POST")
            .uri("/foo/_search")
            .header("content-type", "application/json");
        let builder = match token {
            Some(token) => builder.header("authorization", format!("Bearer {token}")),
            None => builder,
        };
        builder.body(Body::from(r#"{"query":{}}"#)).unwrap()
    }

    #[tokio::test]
    async fn protected_resource_metadata_is_published_when_configured() {
        let app = app_full(
            Arc::new(UnconfiguredProvider),
            None,
            Some(ResourceMetadata {
                resource: "https://logs.example.com".to_owned(),
                authorization_servers: vec!["https://idp.example.com".to_owned()],
                scopes_supported: vec!["logs:read".to_owned()],
            }),
        );

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/.well-known/oauth-protected-resource")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["resource"], "https://logs.example.com");
        assert_eq!(body["authorization_servers"][0], "https://idp.example.com");
        assert_eq!(body["scopes_supported"][0], "logs:read");
        assert_eq!(body["bearer_methods_supported"][0], "header");

        // Without metadata the endpoint does not exist.
        let response = app_with_provider(Arc::new(UnconfiguredProvider))
            .oneshot(
                Request::builder()
                    .uri("/.well-known/oauth-protected-resource")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn jwt_auth_protects_search_but_not_root() {
        let app = app_with_provider_and_auth(
            Arc::new(UnconfiguredProvider),
            Some(Arc::new(auth::test_support::test_authenticator().await)),
        );

        let response = app
            .clone()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let response = app.clone().oneshot(search_request(None)).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.headers()["www-authenticate"],
            "Bearer realm=\"chuanshanjia\""
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(body.is_empty());

        let response = app
            .clone()
            .oneshot(search_request(Some("not-a-jwt")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.headers()["www-authenticate"],
            "Bearer realm=\"chuanshanjia\", error=\"invalid_token\", error_description=\"The access token is invalid or expired\""
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["type"], "invalid_token");
        assert_eq!(body["status"], 401);

        let token = auth::test_support::test_token(
            jsonwebtoken::get_current_timestamp() + 3600,
            Some(auth::test_support::TEST_KID),
        );
        let response = app.oneshot(search_request(Some(&token))).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    }
}
