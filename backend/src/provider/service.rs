use std::{collections::BTreeMap, sync::Arc};

use aliyun_log_rust_sdk::{Client, Config as SlsClientConfig, FromConfig};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use axum::http::StatusCode;
use reqwest::Url;
use serde_json::Value;

use crate::{
    config::{AppConfig, ServiceConfig, StorageConfig},
    provider::{ProviderError, SearchProvider, es::EsProvider, sls::SlsProvider, tls::TlsProvider},
    search::{SearchQuery, SearchRequest},
};

type JsonPath = Vec<String>;

pub struct ServiceProvider {
    services: BTreeMap<String, ConfiguredService>,
}

struct ConfiguredService {
    provider: Arc<dyn SearchProvider>,
    index: String,
    filters: BTreeMap<String, Value>,
    keyword: Option<String>,
    json_fields: Vec<JsonPath>,
    ignores: Vec<JsonPath>,
}

impl ServiceProvider {
    pub fn from_config(config: AppConfig) -> Result<Self> {
        let mut storages: BTreeMap<String, Arc<dyn SearchProvider>> = BTreeMap::new();
        for (name, storage) in config.storages {
            ensure!(!name.is_empty(), "storage name must not be empty");
            let provider = build_storage(&name, storage)?;
            storages.insert(name, provider);
        }
        Self::new(config.services, storages)
    }

    pub fn new(
        services: BTreeMap<String, ServiceConfig>,
        storages: BTreeMap<String, Arc<dyn SearchProvider>>,
    ) -> Result<Self> {
        let mut configured = BTreeMap::new();
        for (name, service) in services {
            ensure!(
                !name.is_empty() && !name.contains('/'),
                "service name must be a nonempty URL path segment"
            );
            ensure!(
                !service.index.is_empty(),
                "service {name:?} index must not be empty"
            );
            let provider = storages.get(&service.storage).cloned().with_context(|| {
                format!(
                    "service {name:?} references unknown storage {:?}",
                    service.storage
                )
            })?;
            let json_fields = parse_paths("json_fields", &service.json_fields)
                .with_context(|| format!("invalid json_fields for service {name:?}"))?;
            let ignores = parse_paths("ignores", &service.ignores)
                .with_context(|| format!("invalid ignores for service {name:?}"))?;
            configured.insert(
                name,
                ConfiguredService {
                    provider,
                    index: service.index,
                    filters: service.filters,
                    keyword: service.keyword,
                    json_fields,
                    ignores,
                },
            );
        }
        Ok(Self {
            services: configured,
        })
    }
}

fn parse_paths(field: &str, paths: &[String]) -> Result<Vec<JsonPath>> {
    paths
        .iter()
        .map(|path| parse_json_path(path).with_context(|| format!("invalid {field} path {path:?}")))
        .collect()
}

fn build_storage(name: &str, storage: StorageConfig) -> Result<Arc<dyn SearchProvider>> {
    match storage {
        StorageConfig::Elasticsearch { base_url } => {
            let url = Url::parse(&base_url)
                .with_context(|| format!("storage {name:?} base_url must be a valid URL"))?;
            ensure!(
                matches!(url.scheme(), "http" | "https"),
                "storage {name:?} base_url must use http or https"
            );
            Ok(Arc::new(EsProvider::new(url)))
        }
        StorageConfig::AliyunSls {
            endpoint,
            project,
            access_key_id,
            access_key_secret,
            security_token,
        } => {
            ensure!(
                !endpoint.is_empty(),
                "storage {name:?} endpoint must not be empty"
            );
            ensure!(
                !project.is_empty(),
                "storage {name:?} project must not be empty"
            );
            let access_key_id = access_key_id.resolve(&format!("storage {name}.access_key_id"))?;
            let access_key_secret =
                access_key_secret.resolve(&format!("storage {name}.access_key_secret"))?;
            let builder = SlsClientConfig::builder().endpoint(endpoint);
            let config = match security_token {
                Some(token) => {
                    let token = token.resolve(&format!("storage {name}.security_token"))?;
                    builder
                        .sts(
                            access_key_id.into_owned(),
                            access_key_secret.into_owned(),
                            token.into_owned(),
                        )
                        .build()
                }
                None => builder
                    .access_key(access_key_id.into_owned(), access_key_secret.into_owned())
                    .build(),
            }
            .with_context(|| format!("invalid SLS configuration for storage {name:?}"))?;
            let client = Client::from_config(config)
                .with_context(|| format!("failed to create SLS client for storage {name:?}"))?;
            Ok(Arc::new(SlsProvider::new(client, project)))
        }
        StorageConfig::VolcesTls {
            region,
            endpoint,
            access_key_id,
            access_key_secret,
            security_token,
        } => {
            ensure!(
                !region.is_empty(),
                "storage {name:?} region must not be empty"
            );
            let endpoint = match endpoint {
                Some(endpoint) => Url::parse(&endpoint)
                    .with_context(|| format!("storage {name:?} endpoint must be a valid URL"))?,
                None => Url::parse(&format!("https://tls-{region}.volces.com")).with_context(|| {
                    format!(
                        "storage {name:?} region {region:?} does not yield a valid TLS endpoint URL"
                    )
                })?,
            };
            ensure!(
                matches!(endpoint.scheme(), "http" | "https"),
                "storage {name:?} endpoint must use http or https"
            );
            let access_key_id = access_key_id
                .resolve(&format!("storage {name}.access_key_id"))?
                .into_owned();
            let access_key_secret = access_key_secret
                .resolve(&format!("storage {name}.access_key_secret"))?
                .into_owned();
            let security_token = match security_token {
                Some(token) => Some(
                    token
                        .resolve(&format!("storage {name}.security_token"))?
                        .into_owned(),
                ),
                None => None,
            };
            Ok(Arc::new(TlsProvider::new(
                endpoint,
                region,
                access_key_id,
                access_key_secret,
                security_token,
            )))
        }
    }
}

fn parse_json_path(path: &str) -> Result<JsonPath> {
    ensure!(!path.is_empty(), "path must not be empty");
    let mut segments = Vec::new();
    let mut offset = 0;
    while offset < path.len() {
        if path.as_bytes()[offset] == b'"' {
            let mut escaped = false;
            let mut end = None;
            for (relative, character) in path[offset + 1..].char_indices() {
                if escaped {
                    escaped = false;
                    continue;
                }
                match character {
                    '\\' => escaped = true,
                    '"' => {
                        end = Some(offset + 1 + relative);
                        break;
                    }
                    _ => {}
                }
            }
            let end = end.context("unterminated quoted path segment")?;
            let segment =
                serde_json::from_str(&path[offset..=end]).context("invalid quoted path segment")?;
            segments.push(segment);
            offset = end + 1;
            if offset < path.len() {
                ensure!(
                    path.as_bytes()[offset] == b'.',
                    "expected '.' after quoted path segment"
                );
                offset += 1;
                ensure!(offset < path.len(), "path must not end with '.'");
            }
        } else {
            let end = path[offset..]
                .find('.')
                .map_or(path.len(), |index| offset + index);
            ensure!(end > offset, "path segments must not be empty");
            let segment = &path[offset..end];
            ensure!(
                !segment.contains('"'),
                "quoted path segments must start with a quote"
            );
            segments.push(segment.to_owned());
            offset = end;
            if offset < path.len() {
                offset += 1;
                ensure!(offset < path.len(), "path must not end with '.'");
            }
        }
    }
    Ok(segments)
}

fn transform_sources(response: &mut Value, mut transform: impl FnMut(&mut Value)) {
    let Some(logs) = response.get_mut("logs").and_then(Value::as_array_mut) else {
        return;
    };
    for log in logs {
        if log.is_object() {
            transform(log);
        }
    }
}

fn parse_json_strings(response: &mut Value, fields: &[JsonPath]) {
    transform_sources(response, |source| {
        if fields.is_empty() {
            if let Some(source) = source.as_object_mut() {
                for value in source.values_mut() {
                    parse_json_container(value);
                }
            }
        } else {
            for path in fields {
                parse_at_path(source, path);
            }
        }
    });
}

fn parse_at_path(value: &mut Value, path: &[String]) {
    let Some((segment, rest)) = path.split_first() else {
        return;
    };
    let Some(child) = value
        .as_object_mut()
        .and_then(|object| object.get_mut(segment))
    else {
        return;
    };
    if rest.is_empty() {
        parse_json_string(child);
    } else {
        parse_json_string(child);
        parse_at_path(child, rest);
    }
}

fn apply_ignores(response: &mut Value, ignores: &[JsonPath]) {
    transform_sources(response, |source| {
        for path in ignores {
            remove_at_path(source, path);
        }
    });
}

fn remove_at_path(value: &mut Value, path: &[String]) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    if object.remove(&path.join(".")).is_some() {
        return;
    }
    let Some((segment, rest)) = path.split_first() else {
        return;
    };
    if rest.is_empty() {
        object.remove(segment);
    } else if let Some(child) = object.get_mut(segment) {
        remove_at_path(child, rest);
    }
}

fn parse_json_container(value: &mut Value) {
    let Value::String(text) = value else {
        return;
    };
    let trimmed = text.trim();
    if (trimmed.starts_with('{') && trimmed.ends_with('}'))
        || (trimmed.starts_with('[') && trimmed.ends_with(']'))
    {
        parse_json_string(value);
    }
}

fn parse_json_string(value: &mut Value) {
    let Value::String(text) = value else {
        return;
    };
    if let Ok(parsed) = serde_json::from_str(text) {
        *value = parsed;
    }
}

#[async_trait]
impl SearchProvider for ServiceProvider {
    async fn search(&self, name: &str, request: &SearchRequest) -> Result<Value, ProviderError> {
        let service = self.services.get(name).ok_or_else(|| {
            ProviderError::new(
                StatusCode::NOT_FOUND,
                "service_not_found",
                format!("search service {name:?} is not configured"),
            )
        })?;
        let mut filter = request.query.filter.clone();
        filter.extend(service.filters.clone());
        let keywords = service
            .keyword
            .clone()
            .or_else(|| request.query.keywords.clone());
        let request = SearchRequest {
            query: SearchQuery { filter, keywords },
            start: request.start,
            end: request.end,
        };

        let mut response = service.provider.search(&service.index, &request).await?;
        parse_json_strings(&mut response, &service.json_fields);
        apply_ignores(&mut response, &service.ignores);
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::{body::to_bytes, response::IntoResponse};
    use serde_json::json;

    use super::*;

    struct RecordedRequest {
        index: String,
        filter: BTreeMap<String, Value>,
        keywords: Option<String>,
        start: Option<i64>,
        end: Option<i64>,
    }

    struct MockProvider {
        response: Value,
        requests: Mutex<Vec<RecordedRequest>>,
    }

    impl MockProvider {
        fn new(response: Value) -> Self {
            Self {
                response,
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl SearchProvider for MockProvider {
        async fn search(
            &self,
            index: &str,
            request: &SearchRequest,
        ) -> Result<Value, ProviderError> {
            self.requests.lock().unwrap().push(RecordedRequest {
                index: index.to_owned(),
                filter: request.query.filter.clone(),
                keywords: request.query.keywords.clone(),
                start: request.start,
                end: request.end,
            });
            Ok(self.response.clone())
        }
    }

    fn service() -> ServiceConfig {
        ServiceConfig {
            storage: "pre".to_owned(),
            index: "app-logs".to_owned(),
            filters: BTreeMap::new(),
            keyword: None,
            json_fields: Vec::new(),
            ignores: Vec::new(),
            lookups: Vec::new(),
        }
    }

    fn provider(service: ServiceConfig, mock: Arc<MockProvider>) -> ServiceProvider {
        let storage = service.storage.clone();
        let search_provider: Arc<dyn SearchProvider> = mock;
        ServiceProvider::new(
            BTreeMap::from([("foo".to_owned(), service)]),
            BTreeMap::from([(storage, search_provider)]),
        )
        .unwrap()
    }

    fn request(query: Value) -> SearchRequest {
        serde_json::from_value(json!({"query": query})).unwrap()
    }

    #[tokio::test]
    async fn preset_query_takes_precedence_over_request() {
        let mut service = service();
        service.filters = BTreeMap::from([
            ("environment".to_owned(), json!("pre")),
            ("service".to_owned(), json!("order")),
        ]);
        service.keyword = Some("preset".to_owned());
        let upstream = json!({"logs": []});
        let mock = Arc::new(MockProvider::new(upstream.clone()));
        let provider = provider(service, mock.clone());

        let result = provider
            .search(
                "foo",
                &request(json!({
                    "filter": {"service": "checkout", "trace_id": "abc"}
                })),
            )
            .await
            .unwrap();
        assert_eq!(result, upstream);
        provider
            .search("foo", &request(json!({"keywords": "request"})))
            .await
            .unwrap();
        let mut timed_request = request(json!({}));
        timed_request.start = Some(1760000000);
        timed_request.end = Some(1760003600);
        provider.search("foo", &timed_request).await.unwrap();

        let requests = mock.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].index, "app-logs");
        assert_eq!(
            requests[0].filter,
            BTreeMap::from([
                ("environment".to_owned(), json!("pre")),
                ("service".to_owned(), json!("order")),
                ("trace_id".to_owned(), json!("abc")),
            ])
        );
        assert_eq!(requests[0].keywords.as_deref(), Some("preset"));
        assert_eq!(requests[1].filter["service"], "order");
        assert_eq!(requests[1].filter.len(), 2);
        assert_eq!(requests[1].keywords.as_deref(), Some("preset"));
        assert_eq!(requests[2].start, Some(1760000000));
        assert_eq!(requests[2].end, Some(1760003600));
    }

    #[tokio::test]
    async fn request_query_is_used_where_service_has_no_preset() {
        let mut service = service();
        service.filters = BTreeMap::from([("environment".to_owned(), json!("pre"))]);
        let mock = Arc::new(MockProvider::new(json!({})));
        let provider = provider(service, mock.clone());

        provider
            .search(
                "foo",
                &request(json!({
                    "filter": {"service": "checkout"},
                    "keywords": "request"
                })),
            )
            .await
            .unwrap();

        let requests = mock.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].filter,
            BTreeMap::from([
                ("environment".to_owned(), json!("pre")),
                ("service".to_owned(), json!("checkout")),
            ])
        );
        assert_eq!(requests[0].keywords.as_deref(), Some("request"));
    }

    #[tokio::test]
    async fn unknown_service_returns_not_found_without_calling_storage() {
        let mock = Arc::new(MockProvider::new(json!({})));
        let provider = provider(service(), mock.clone());

        let error = provider
            .search("missing", &request(json!({})))
            .await
            .unwrap_err();
        let response = error.into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["type"], "service_not_found");
        assert!(mock.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn ignores_are_applied_and_lookups_have_no_runtime_effect() {
        let mut service = service();
        service.ignores = vec!["service".to_owned(), "thread.id".to_owned()];
        service.lookups = vec!["missing".to_owned()];
        let mock = Arc::new(MockProvider::new(json!({
            "logs": [{"service": "order", "thread.id": "worker", "keep": true}]
        })));
        let provider = provider(service, mock.clone());

        let result = provider.search("foo", &request(json!({}))).await.unwrap();

        assert_eq!(result, json!({"logs": [{"keep": true}]}));
        assert_eq!(mock.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn ignores_support_literal_and_quoted_dotted_fields() {
        for ignored in ["otlp.name", "\"otlp.name\""] {
            let mut service = service();
            service.ignores = vec![ignored.to_owned()];
            let mock = Arc::new(MockProvider::new(json!({
                "logs": [{
                    "otlp.name": "literal",
                    "otlp": {"name": "nested"},
                    "keep": true
                }]
            })));
            let provider = provider(service, mock);

            let result = provider.search("foo", &request(json!({}))).await.unwrap();

            let source = &result["logs"][0];
            assert!(source.get("otlp.name").is_none(), "failed for {ignored}");
            assert_eq!(source["otlp"]["name"], "nested");
            assert_eq!(source["keep"], true);
        }
    }

    #[tokio::test]
    async fn json_containers_are_automatically_detected() {
        let attribute = serde_json::to_string(&json!({
            "kv.seller_id": "123",
            "enabled": true,
            "nested": "{\"inner\":true}"
        }))
        .unwrap();
        let mock = Arc::new(MockProvider::new(json!({
            "logs": [{
                "attribute": attribute,
                "resource": "{\"deployment.environment\":\"test\"}",
                "array": "[1,2]",
                "invalid": "{not-json}",
                "number": "42",
                "plain": "text"
            }]
        })));
        let provider = provider(service(), mock);

        let result = provider.search("foo", &request(json!({}))).await.unwrap();

        let source = &result["logs"][0];
        assert_eq!(source["attribute"]["kv.seller_id"], "123");
        assert_eq!(source["attribute"]["enabled"], true);
        assert_eq!(source["attribute"]["nested"], "{\"inner\":true}");
        assert_eq!(source["resource"]["deployment.environment"], "test");
        assert_eq!(source["array"], json!([1, 2]));
        assert_eq!(source["invalid"], "{not-json}");
        assert_eq!(source["number"], "42");
        assert_eq!(source["plain"], "text");
    }

    #[tokio::test]
    async fn configured_json_fields_limit_conversion() {
        let mut service = service();
        service.json_fields = vec![
            "attribute".to_owned(),
            "resource".to_owned(),
            "missing".to_owned(),
        ];
        let mock = Arc::new(MockProvider::new(json!({
            "logs": [{
                "attribute": "{\"key\":\"value\"}",
                "resource": "not-json",
                "other": "{\"automatic\":false}"
            }]
        })));
        let provider = provider(service, mock);

        let result = provider.search("foo", &request(json!({}))).await.unwrap();

        let source = &result["logs"][0];
        assert_eq!(source["attribute"], json!({"key": "value"}));
        assert_eq!(source["resource"], "not-json");
        assert_eq!(source["other"], "{\"automatic\":false}");
    }

    #[tokio::test]
    async fn nested_json_fields_are_parsed() {
        let mut service = service();
        service.json_fields = vec!["attribute".to_owned(), "attribute.\"kv.legacy\"".to_owned()];
        let legacy = serde_json::to_string(&json!({
            "body": {"content": "nested"},
            "seller_id": "123"
        }))
        .unwrap();
        let attribute = serde_json::to_string(&json!({"kv.legacy": legacy})).unwrap();
        let mock = Arc::new(MockProvider::new(json!({
            "logs": [{
                "attribute": attribute,
                "other": "{\"automatic\":false}"
            }]
        })));
        let provider = provider(service, mock);

        let result = provider.search("foo", &request(json!({}))).await.unwrap();

        let source = &result["logs"][0];
        assert_eq!(
            source["attribute"]["kv.legacy"]["body"]["content"],
            "nested"
        );
        assert_eq!(source["attribute"]["kv.legacy"]["seller_id"], "123");
        assert_eq!(source["other"], "{\"automatic\":false}");
    }

    #[tokio::test]
    async fn ignores_are_applied_after_json_parsing() {
        let mut service = service();
        service.json_fields = vec!["attribute".to_owned(), "attribute.\"kv.legacy\"".to_owned()];
        service.ignores = vec![
            "attribute.\"thread.name\"".to_owned(),
            "attribute.\"kv.legacy\".body".to_owned(),
            "__pack_meta__".to_owned(),
            "missing.path".to_owned(),
            "array.0".to_owned(),
        ];
        let legacy = serde_json::to_string(&json!({
            "body": {"content": "remove me"},
            "seller_id": "123"
        }))
        .unwrap();
        let attribute = serde_json::to_string(&json!({
            "kv.legacy": legacy,
            "thread.name": "worker",
            "thread.id": 1
        }))
        .unwrap();
        let mock = Arc::new(MockProvider::new(json!({
            "logs": [{
                "attribute": attribute,
                "array": [1, 2],
                "__pack_meta__": "meta",
                "keep": true
            }]
        })));
        let provider = provider(service, mock);

        let result = provider.search("foo", &request(json!({}))).await.unwrap();

        let source = &result["logs"][0];
        assert!(source["attribute"].get("thread.name").is_none());
        assert_eq!(source["attribute"]["thread.id"], 1);
        assert!(source["attribute"]["kv.legacy"].get("body").is_none());
        assert_eq!(source["attribute"]["kv.legacy"]["seller_id"], "123");
        assert!(source.get("__pack_meta__").is_none());
        assert_eq!(source["array"], json!([1, 2]));
        assert_eq!(source["keep"], true);
    }

    #[test]
    fn json_path_supports_quoted_segments_and_escapes() {
        assert_eq!(
            parse_json_path("attribute.\"kv.legacy\"").unwrap(),
            ["attribute", "kv.legacy"]
        );
        assert_eq!(
            parse_json_path("attribute.\"say\\\"hello\"").unwrap(),
            ["attribute", "say\"hello"]
        );

        for path in [
            "",
            ".",
            "attribute.",
            "attribute..kv",
            "attribute.\"kv",
            "attribute.\"kv\"legacy",
        ] {
            assert!(parse_json_path(path).is_err(), "accepted {path:?}");
        }
    }

    #[test]
    fn invalid_ignores_path_is_rejected_at_startup() {
        let mut service = service();
        service.ignores = vec!["attribute.".to_owned()];
        let mock: Arc<dyn SearchProvider> = Arc::new(MockProvider::new(json!({})));
        let Err(error) = ServiceProvider::new(
            BTreeMap::from([("foo".to_owned(), service)]),
            BTreeMap::from([("pre".to_owned(), mock)]),
        ) else {
            panic!("invalid ignores path must be rejected");
        };

        assert!(format!("{error:?}").contains("invalid ignores path"));
    }

    #[test]
    fn unknown_storage_is_rejected_at_startup() {
        let Err(error) = ServiceProvider::new(
            BTreeMap::from([("foo".to_owned(), service())]),
            BTreeMap::new(),
        ) else {
            panic!("unknown storage must be rejected");
        };

        assert!(error.to_string().contains("unknown storage"));
    }
}
