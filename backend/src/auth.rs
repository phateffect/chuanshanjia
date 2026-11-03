use std::{
    collections::HashSet,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header::AUTHORIZATION, header::WWW_AUTHENTICATE},
    middleware::Next,
    response::{IntoResponse, Response},
};
use jsonwebtoken::{
    Algorithm, DecodingKey, Validation, decode, decode_header, get_current_timestamp,
    jwk::{
        AlgorithmParameters, EllipticCurve, Jwk, JwkSet, KeyAlgorithm, KeyOperations, PublicKeyUse,
    },
};
use moka::sync::Cache;
use reqwest::Url;
use serde::Deserialize;
use serde_json::Value;

use crate::provider::ProviderError;

const CACHE_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const JWKS_REFRESH_MIN_INTERVAL: Duration = Duration::from_secs(30);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

pub struct JwtAuthenticator {
    keys: RwLock<Vec<VerificationKey>>,
    issuer: String,
    jwks_uri: Url,
    fetcher: Arc<dyn JsonFetcher>,
    refresh_min_interval: Duration,
    last_refresh: Mutex<Option<Instant>>,
    validation: Validation,
    required_scopes: Vec<String>,
    cache: Cache<String, CachedToken>,
}

struct VerificationKey {
    kid: Option<String>,
    key: DecodingKey,
}

#[derive(Clone, Copy)]
struct CachedToken {
    exp: Option<u64>,
}

#[async_trait]
trait JsonFetcher: Send + Sync {
    async fn fetch_json(&self, url: &Url) -> Result<Value>;
}

#[async_trait]
impl JsonFetcher for reqwest::Client {
    async fn fetch_json(&self, url: &Url) -> Result<Value> {
        let response = self
            .get(url.clone())
            .send()
            .await
            .with_context(|| format!("failed to GET {url}"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .with_context(|| format!("failed to read response from {url}"))?;
        ensure!(status.is_success(), "GET {url} returned {status}: {body}");
        serde_json::from_str(&body).with_context(|| format!("failed to parse JSON from {url}"))
    }
}

#[derive(Deserialize)]
struct Claims {
    exp: u64,
    iss: Option<String>,
    /// RFC 9068 uses a space-delimited string; some IdPs emit an array instead.
    #[serde(default)]
    scope: Option<ScopeClaim>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ScopeClaim {
    Text(String),
    List(Vec<String>),
}

impl ScopeClaim {
    fn granted(&self) -> Vec<&str> {
        match self {
            Self::Text(text) => text.split_whitespace().collect(),
            Self::List(list) => list.iter().map(String::as_str).collect(),
        }
    }
}

#[derive(Deserialize)]
struct DiscoveryDocument {
    issuer: String,
    jwks_uri: String,
}

impl JwtAuthenticator {
    /// Discover and load the JWKS of an OIDC issuer at construction time.
    pub async fn from_oidc_issuer(
        issuer: &str,
        audiences: &[String],
        required_scopes: &[String],
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .context("failed to build HTTP client for OIDC discovery")?;
        Self::from_oidc_issuer_with_fetcher(
            issuer,
            audiences,
            required_scopes,
            Arc::new(client),
            JWKS_REFRESH_MIN_INTERVAL,
            CACHE_IDLE_TIMEOUT,
        )
        .await
    }

    async fn from_oidc_issuer_with_fetcher(
        issuer: &str,
        audiences: &[String],
        required_scopes: &[String],
        fetcher: Arc<dyn JsonFetcher>,
        refresh_min_interval: Duration,
        idle_timeout: Duration,
    ) -> Result<Self> {
        let issuer = normalize_issuer(issuer)?;
        let discovery_url = Url::parse(&format!("{issuer}/.well-known/openid-configuration"))
            .with_context(|| format!("failed to build the discovery URL for issuer {issuer:?}"))?;
        let document: DiscoveryDocument =
            serde_json::from_value(fetcher.fetch_json(&discovery_url).await.with_context(
                || format!("failed to load OIDC discovery document {discovery_url}"),
            )?)
            .with_context(|| format!("invalid OIDC discovery document at {discovery_url}"))?;
        ensure!(
            document.issuer.trim_end_matches('/') == issuer,
            "OIDC discovery document issuer {:?} does not match configured issuer {issuer:?}",
            document.issuer,
        );
        let jwks_uri = Url::parse(&document.jwks_uri).with_context(|| {
            format!(
                "invalid jwks_uri {:?} in OIDC discovery document",
                document.jwks_uri
            )
        })?;
        ensure!(
            matches!(jwks_uri.scheme(), "http" | "https"),
            "OIDC jwks_uri {jwks_uri} must use http or https"
        );
        let jwks = fetch_jwks(&fetcher, &jwks_uri).await?;
        Ok(Self {
            keys: RwLock::new(build_keys(&jwks)?),
            issuer,
            jwks_uri,
            fetcher,
            refresh_min_interval,
            last_refresh: Mutex::new(None),
            validation: eddsa_validation(audiences),
            required_scopes: required_scopes.to_vec(),
            cache: Cache::builder().time_to_idle(idle_timeout).build(),
        })
    }

    pub async fn authenticate(&self, token: &str) -> std::result::Result<(), AuthError> {
        self.authenticate_at(token, get_current_timestamp()).await
    }

    async fn authenticate_at(&self, token: &str, now: u64) -> std::result::Result<(), AuthError> {
        if let Some(cached) = self.cache.get(token) {
            return match cached.exp {
                Some(exp) if exp >= now => Ok(()),
                Some(_) => {
                    self.cache
                        .insert(token.to_owned(), CachedToken { exp: None });
                    Err(AuthError::invalid_token())
                }
                None => Err(AuthError::invalid_token()),
            };
        }

        let result = self.verify(token).await;
        self.cache.insert(
            token.to_owned(),
            CachedToken {
                exp: result.as_ref().ok().copied(),
            },
        );
        result.map(|_| ())
    }

    async fn verify(&self, token: &str) -> std::result::Result<u64, AuthError> {
        let header = decode_header(token).map_err(|_| AuthError::invalid_token())?;
        if header.alg != Algorithm::EdDSA {
            return Err(AuthError::invalid_token());
        }
        let kid = header.kid.as_deref();
        let mut key = self.select_key(kid);
        if key.is_none() {
            self.refresh_jwks().await;
            key = self.select_key(kid);
        }
        let key = key.ok_or_else(AuthError::invalid_token)?;
        let token_data = decode::<Claims>(token, &key, &self.validation)
            .map_err(|_| AuthError::invalid_token())?;
        if token_data.claims.iss.as_deref() != Some(self.issuer.as_str()) {
            return Err(AuthError::invalid_token());
        }
        if !self.required_scopes.is_empty() {
            let granted = token_data
                .claims
                .scope
                .as_ref()
                .map(ScopeClaim::granted)
                .unwrap_or_default();
            if self
                .required_scopes
                .iter()
                .any(|required| !granted.contains(&required.as_str()))
            {
                return Err(AuthError::invalid_token());
            }
        }
        Ok(token_data.claims.exp)
    }

    fn select_key(&self, kid: Option<&str>) -> Option<DecodingKey> {
        let keys = self.keys.read().unwrap();
        keys.iter()
            .find(|candidate| match kid {
                Some(kid) => candidate.kid.as_deref() == Some(kid),
                None => keys.len() == 1,
            })
            .map(|candidate| candidate.key.clone())
    }

    async fn refresh_jwks(&self) {
        {
            let mut last_refresh = self.last_refresh.lock().unwrap();
            if last_refresh.is_some_and(|at| at.elapsed() < self.refresh_min_interval) {
                return;
            }
            *last_refresh = Some(Instant::now());
        }
        match fetch_jwks(&self.fetcher, &self.jwks_uri).await {
            Ok(jwks) => match build_keys(&jwks) {
                Ok(keys) => *self.keys.write().unwrap() = keys,
                Err(error) => {
                    eprintln!(
                        "failed to use refreshed JWKS from {}: {error:#}",
                        self.jwks_uri
                    );
                }
            },
            Err(error) => {
                eprintln!("failed to refresh JWKS from {}: {error:#}", self.jwks_uri);
            }
        }
    }
}

async fn fetch_jwks(fetcher: &Arc<dyn JsonFetcher>, jwks_uri: &Url) -> Result<JwkSet> {
    let value = fetcher
        .fetch_json(jwks_uri)
        .await
        .with_context(|| format!("failed to load JWKS from {jwks_uri}"))?;
    serde_json::from_value(value).with_context(|| format!("invalid JWKS at {jwks_uri}"))
}

fn normalize_issuer(issuer: &str) -> Result<String> {
    let url = Url::parse(issuer).with_context(|| format!("invalid OIDC issuer {issuer:?}"))?;
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "OIDC issuer must use http or https"
    );
    Ok(issuer.trim_end_matches('/').to_owned())
}

fn eddsa_validation(audiences: &[String]) -> Validation {
    let mut validation = Validation::new(Algorithm::EdDSA);
    validation.leeway = 0;
    validation.validate_nbf = true;
    // A JWT access token must name one of the resource identifiers configured
    // for this backend.
    validation.set_audience(audiences);
    validation.set_required_spec_claims(&["exp", "aud"]);
    validation
}

fn build_keys(jwks: &JwkSet) -> Result<Vec<VerificationKey>> {
    let mut keys = Vec::new();
    let mut key_ids = HashSet::new();
    for (index, jwk) in jwks.keys.iter().enumerate() {
        if !is_ed25519_verification_key(jwk) {
            continue;
        }
        ensure!(
            jwk.common
                .key_id
                .as_deref()
                .is_none_or(|kid| !kid.is_empty()),
            "JWKS key at index {index} has an empty kid"
        );
        if let Some(kid) = &jwk.common.key_id {
            ensure!(
                key_ids.insert(kid.clone()),
                "JWKS contains duplicate kid {kid:?}"
            );
        }
        let key = DecodingKey::from_jwk(jwk)
            .with_context(|| format!("invalid Ed25519 JWKS key at index {index}"))?;
        ensure!(
            key.try_get_as_bytes()?.len() == 32,
            "Ed25519 JWKS key at index {index} must contain a 32-byte public key"
        );
        keys.push(VerificationKey {
            kid: jwk.common.key_id.clone(),
            key,
        });
    }
    ensure!(
        !keys.is_empty(),
        "JWKS must contain at least one Ed25519 verification key"
    );

    Ok(keys)
}

fn is_ed25519_verification_key(jwk: &Jwk) -> bool {
    let AlgorithmParameters::OctetKeyPair(parameters) = &jwk.algorithm else {
        return false;
    };
    parameters.curve == EllipticCurve::Ed25519
        && matches!(jwk.common.key_algorithm, None | Some(KeyAlgorithm::EdDSA))
        && matches!(
            jwk.common.public_key_use.as_ref(),
            None | Some(PublicKeyUse::Signature)
        )
        && jwk
            .common
            .key_operations
            .as_ref()
            .is_none_or(|operations| operations.contains(&KeyOperations::Verify))
}

const MISSING_TOKEN_CHALLENGE: &str = "Bearer realm=\"chuanshanjia\"";
const INVALID_TOKEN_CHALLENGE: &str = "Bearer realm=\"chuanshanjia\", error=\"invalid_token\", error_description=\"The access token is invalid or expired\"";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthError {
    kind: AuthErrorKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuthErrorKind {
    Missing,
    InvalidToken,
}

impl AuthError {
    fn missing() -> Self {
        Self {
            kind: AuthErrorKind::Missing,
        }
    }

    fn invalid_token() -> Self {
        Self {
            kind: AuthErrorKind::InvalidToken,
        }
    }
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let mut response = match self.kind {
            AuthErrorKind::Missing => StatusCode::UNAUTHORIZED.into_response(),
            AuthErrorKind::InvalidToken => ProviderError::new(
                StatusCode::UNAUTHORIZED,
                "invalid_token",
                "The access token is invalid or expired",
            )
            .into_response(),
        };
        let challenge = match self.kind {
            AuthErrorKind::Missing => MISSING_TOKEN_CHALLENGE,
            AuthErrorKind::InvalidToken => INVALID_TOKEN_CHALLENGE,
        };
        response
            .headers_mut()
            .insert(WWW_AUTHENTICATE, HeaderValue::from_static(challenge));
        response
    }
}

fn bearer_token(headers: &HeaderMap) -> std::result::Result<&str, AuthError> {
    let value = headers.get(AUTHORIZATION).ok_or_else(AuthError::missing)?;
    let value = value.to_str().map_err(|_| AuthError::invalid_token())?;
    let (scheme, token) = value.split_once(' ').ok_or_else(AuthError::missing)?;
    if !scheme.eq_ignore_ascii_case("bearer") || token.is_empty() {
        return Err(AuthError::missing());
    }
    if token.chars().any(char::is_whitespace) {
        return Err(AuthError::invalid_token());
    }
    Ok(token)
}

pub async fn require_jwt(
    State(authenticator): State<Arc<JwtAuthenticator>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let result = match bearer_token(request.headers()) {
        Ok(token) => authenticator.authenticate(token).await,
        Err(error) => Err(error),
    };
    match result {
        Ok(()) => next.run(request).await,
        Err(error) => error.into_response(),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::{BTreeMap, VecDeque};

    use jsonwebtoken::{EncodingKey, Header, encode};
    use serde_json::{Value, json};

    use super::*;

    pub(crate) const TEST_ISSUER: &str = "https://idp.example.com";
    pub(crate) const TEST_DISCOVERY_URL: &str =
        "https://idp.example.com/.well-known/openid-configuration";
    pub(crate) const TEST_JWKS_URL: &str = "https://idp.example.com/jwks";
    pub(crate) const TEST_KID: &str = "test-key";
    pub(crate) const TEST_AUDIENCE: &str = "test-audience";
    const TEST_PRIVATE_KEY_DER: &[u8] = &[
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20, 0xd4, 0xee, 0x72, 0xdb, 0xf9, 0x13, 0x58, 0x4a, 0xd5, 0xb6, 0xd8, 0xf1, 0xf7, 0x69,
        0xf8, 0xad, 0x3a, 0xfe, 0x7c, 0x28, 0xcb, 0xf1, 0xd4, 0xfb, 0xe0, 0x97, 0xa8, 0x8f, 0x44,
        0x75, 0x58, 0x42,
    ];

    fn encoding_key() -> EncodingKey {
        EncodingKey::from_ed_der(TEST_PRIVATE_KEY_DER)
    }

    pub(crate) fn test_jwk(kid: Option<&str>) -> Jwk {
        let mut jwk = Jwk::from_encoding_key(&encoding_key(), Algorithm::EdDSA).unwrap();
        jwk.common.key_id = kid.map(str::to_owned);
        jwk
    }

    pub(crate) fn test_jwks() -> JwkSet {
        JwkSet {
            keys: vec![test_jwk(Some(TEST_KID))],
        }
    }

    /// A structurally valid JWKS that shares no key with [`test_jwks`]; used to
    /// exercise unknown-kid handling without a second private key.
    pub(crate) fn standby_jwks() -> JwkSet {
        let mut jwk = test_jwk(Some("standby-key"));
        let AlgorithmParameters::OctetKeyPair(parameters) = &mut jwk.algorithm else {
            panic!("expected an Ed25519 key");
        };
        parameters.x = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8".to_owned();
        JwkSet { keys: vec![jwk] }
    }

    pub(crate) fn test_discovery() -> Value {
        json!({"issuer": TEST_ISSUER, "jwks_uri": TEST_JWKS_URL})
    }

    pub(crate) async fn test_authenticator() -> JwtAuthenticator {
        authenticator_with_jwks_value(serde_json::to_value(test_jwks()).unwrap())
            .await
            .expect("test OIDC issuer should be loadable")
    }

    pub(crate) async fn authenticator_with_jwks_value(jwks: Value) -> Result<JwtAuthenticator> {
        authenticator_with(jwks, &[]).await
    }

    pub(crate) async fn authenticator_with(
        jwks: Value,
        required_scopes: &[&str],
    ) -> Result<JwtAuthenticator> {
        let fetcher = Arc::new(MockFetcher::new(&[
            (TEST_DISCOVERY_URL, vec![test_discovery()]),
            (TEST_JWKS_URL, vec![jwks]),
        ]));
        JwtAuthenticator::from_oidc_issuer_with_fetcher(
            TEST_ISSUER,
            &[TEST_AUDIENCE.to_owned()],
            &required_scopes
                .iter()
                .map(|scope| (*scope).to_owned())
                .collect::<Vec<_>>(),
            fetcher,
            JWKS_REFRESH_MIN_INTERVAL,
            CACHE_IDLE_TIMEOUT,
        )
        .await
    }

    pub(crate) fn test_token(exp: u64, kid: Option<&str>) -> String {
        test_token_with_claims(
            json!({"exp": exp, "sub": "test", "iss": TEST_ISSUER, "aud": TEST_AUDIENCE}),
            kid,
        )
    }

    pub(crate) fn test_token_with_claims(claims: Value, kid: Option<&str>) -> String {
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = kid.map(str::to_owned);
        encode(&header, &claims, &encoding_key()).unwrap()
    }

    /// Routes each URL to a queue of responses; queued responses are consumed
    /// in order and the last one repeats forever.
    #[derive(Default)]
    pub(crate) struct MockFetcher {
        routes: Mutex<BTreeMap<String, VecDeque<Value>>>,
        calls: Mutex<Vec<String>>,
    }

    impl MockFetcher {
        pub(crate) fn new(routes: &[(&str, Vec<Value>)]) -> Self {
            Self {
                routes: Mutex::new(
                    routes
                        .iter()
                        .map(|(url, responses)| ((*url).to_owned(), responses.clone().into()))
                        .collect(),
                ),
                calls: Mutex::new(Vec::new()),
            }
        }

        pub(crate) fn calls_of(&self, url: &str) -> usize {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.as_str() == url)
                .count()
        }
    }

    #[async_trait]
    impl JsonFetcher for MockFetcher {
        async fn fetch_json(&self, url: &Url) -> anyhow::Result<Value> {
            let url = url.to_string();
            self.calls.lock().unwrap().push(url.clone());
            let mut routes = self.routes.lock().unwrap();
            let queue = routes
                .get_mut(&url)
                .with_context(|| format!("unexpected fetch of {url}"))?;
            if queue.len() > 1 {
                queue.pop_front()
            } else {
                queue.front().cloned()
            }
            .with_context(|| format!("no response configured for {url}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use jsonwebtoken::{EncodingKey, Header, encode};
    use serde_json::json;

    use super::{
        test_support::{
            MockFetcher, TEST_AUDIENCE, TEST_DISCOVERY_URL, TEST_ISSUER, TEST_JWKS_URL, TEST_KID,
            authenticator_with_jwks_value, standby_jwks, test_authenticator, test_discovery,
            test_jwk, test_jwks, test_token, test_token_with_claims,
        },
        *,
    };

    /// Blocks JWKS refresh for the foreseeable future; used by tests that probe
    /// the cache by clearing the loaded keys.
    fn block_refresh(authenticator: &JwtAuthenticator) {
        *authenticator.last_refresh.lock().unwrap() = Some(Instant::now());
    }

    async fn oidc_authenticator(
        jwks: &JwkSet,
        refresh_min_interval: Duration,
        idle_timeout: Duration,
    ) -> JwtAuthenticator {
        let fetcher = Arc::new(MockFetcher::new(&[
            (TEST_DISCOVERY_URL, vec![test_discovery()]),
            (TEST_JWKS_URL, vec![serde_json::to_value(jwks).unwrap()]),
        ]));
        JwtAuthenticator::from_oidc_issuer_with_fetcher(
            TEST_ISSUER,
            &[TEST_AUDIENCE.to_owned()],
            &[],
            fetcher,
            refresh_min_interval,
            idle_timeout,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn valid_and_invalid_tokens_use_thirty_minute_sliding_cache() {
        let authenticator = test_authenticator().await;
        block_refresh(&authenticator);
        assert_eq!(
            authenticator.cache.policy().time_to_idle(),
            Some(CACHE_IDLE_TIMEOUT)
        );
        assert_eq!(authenticator.cache.policy().time_to_live(), None);
        let valid = test_token(get_current_timestamp() + 3600, Some(TEST_KID));
        let invalid = "not-a-jwt";
        authenticator.authenticate(&valid).await.unwrap();
        assert!(authenticator.authenticate(invalid).await.is_err());
        assert!(matches!(
            authenticator.cache.get(invalid),
            Some(CachedToken { exp: None })
        ));
        authenticator.keys.write().unwrap().clear();

        authenticator.authenticate(&valid).await.unwrap();
        assert!(matches!(
            authenticator.cache.get(invalid),
            Some(CachedToken { exp: None })
        ));
    }

    #[tokio::test]
    async fn cache_entries_expire_after_idle_timeout() {
        let authenticator = oidc_authenticator(
            &test_jwks(),
            Duration::from_secs(3600),
            Duration::from_millis(20),
        )
        .await;
        block_refresh(&authenticator);
        let token = test_token(get_current_timestamp() + 3600, Some(TEST_KID));
        authenticator.authenticate(&token).await.unwrap();
        authenticator.keys.write().unwrap().clear();
        std::thread::sleep(Duration::from_millis(75));

        assert!(authenticator.authenticate(&token).await.is_err());
    }

    #[tokio::test]
    async fn reads_renew_cache_entries() {
        let authenticator = oidc_authenticator(
            &test_jwks(),
            Duration::from_secs(3600),
            Duration::from_secs(1),
        )
        .await;
        block_refresh(&authenticator);
        let token = test_token(get_current_timestamp() + 3600, Some(TEST_KID));
        authenticator.authenticate(&token).await.unwrap();
        authenticator.keys.write().unwrap().clear();

        std::thread::sleep(Duration::from_millis(600));
        authenticator.authenticate(&token).await.unwrap();
        std::thread::sleep(Duration::from_millis(600));
        authenticator.authenticate(&token).await.unwrap();
    }

    #[tokio::test]
    async fn cached_token_is_rejected_after_its_own_expiration() {
        let authenticator = test_authenticator().await;
        let token = test_token(get_current_timestamp() + 3600, Some(TEST_KID));
        authenticator
            .cache
            .insert(token.clone(), CachedToken { exp: Some(100) });

        let error = authenticator
            .authenticate_at(&token, 101)
            .await
            .unwrap_err();
        assert_eq!(error.kind, AuthErrorKind::InvalidToken);
        assert!(matches!(
            authenticator.cache.get(&token),
            Some(CachedToken { exp: None })
        ));
    }

    #[tokio::test]
    async fn invalid_signatures_and_algorithms_are_rejected() {
        let authenticator = test_authenticator().await;
        let token = test_token(get_current_timestamp() + 3600, Some(TEST_KID));
        let mut tampered = token.into_bytes();
        let signature_start = tampered.iter().rposition(|byte| *byte == b'.').unwrap() + 1;
        tampered[signature_start] = if tampered[signature_start] == b'A' {
            b'B'
        } else {
            b'A'
        };
        let tampered = String::from_utf8(tampered).unwrap();
        assert!(authenticator.authenticate(&tampered).await.is_err());

        let wrong_algorithm = encode(
            &Header::new(Algorithm::HS256),
            &json!({"exp": get_current_timestamp() + 3600}),
            &EncodingKey::from_secret(b"secret"),
        )
        .unwrap();
        assert!(authenticator.authenticate(&wrong_algorithm).await.is_err());
    }

    #[tokio::test]
    async fn required_time_claims_are_validated() {
        let authenticator = test_authenticator().await;
        let now = get_current_timestamp();
        for token in [
            test_token(now.saturating_sub(1), Some(TEST_KID)),
            test_token_with_claims(json!({"sub": "missing exp"}), Some(TEST_KID)),
            test_token_with_claims(
                json!({"exp": now + 3600, "nbf": now + 60, "iss": TEST_ISSUER}),
                Some(TEST_KID),
            ),
        ] {
            assert!(authenticator.authenticate(&token).await.is_err());
        }

        let wrong_audience = test_token_with_claims(
            json!({"exp": now + 3600, "aud": "any-audience", "iss": TEST_ISSUER}),
            Some(TEST_KID),
        );
        assert!(authenticator.authenticate(&wrong_audience).await.is_err());
    }

    #[tokio::test]
    async fn required_scopes_must_be_granted() {
        let fetcher = Arc::new(MockFetcher::new(&[
            (TEST_DISCOVERY_URL, vec![test_discovery()]),
            (
                TEST_JWKS_URL,
                vec![serde_json::to_value(test_jwks()).unwrap()],
            ),
        ]));
        let authenticator = JwtAuthenticator::from_oidc_issuer_with_fetcher(
            TEST_ISSUER,
            &[TEST_AUDIENCE.to_owned()],
            &["logs:read".to_owned(), "logs:export".to_owned()],
            fetcher,
            Duration::from_secs(3600),
            CACHE_IDLE_TIMEOUT,
        )
        .await
        .unwrap();
        let now = get_current_timestamp();
        let token = |scope: serde_json::Value| {
            test_token_with_claims(
                json!({
                    "exp": now + 3600,
                    "iss": TEST_ISSUER,
                    "aud": TEST_AUDIENCE,
                    "scope": scope
                }),
                Some(TEST_KID),
            )
        };

        // A space-delimited string, an array, and extra scopes all pass.
        for scope in [
            json!("logs:read logs:export"),
            json!(["logs:export", "logs:read"]),
            json!("openid logs:read logs:export extra"),
        ] {
            authenticator.authenticate(&token(scope)).await.unwrap();
        }

        // Missing one of the required scopes, or no scope claim at all, fails.
        for scope in [json!("logs:read"), json!("openid logs:read"), json!("")] {
            assert!(authenticator.authenticate(&token(scope)).await.is_err());
        }
        let missing = test_token_with_claims(
            json!({"exp": now + 3600, "iss": TEST_ISSUER, "aud": TEST_AUDIENCE}),
            Some(TEST_KID),
        );
        assert!(authenticator.authenticate(&missing).await.is_err());

        // With no required scopes configured, the claim is not inspected.
        let permissive = authenticator_with_jwks_value(serde_json::to_value(test_jwks()).unwrap())
            .await
            .unwrap();
        permissive.authenticate(&missing).await.unwrap();
    }

    #[tokio::test]
    async fn audience_must_match_the_configured_values() {
        let fetcher = Arc::new(MockFetcher::new(&[
            (TEST_DISCOVERY_URL, vec![test_discovery()]),
            (
                TEST_JWKS_URL,
                vec![serde_json::to_value(test_jwks()).unwrap()],
            ),
        ]));
        let authenticator = JwtAuthenticator::from_oidc_issuer_with_fetcher(
            TEST_ISSUER,
            &["https://logs.example.com".to_owned()],
            &[],
            fetcher,
            Duration::from_secs(3600),
            CACHE_IDLE_TIMEOUT,
        )
        .await
        .unwrap();
        let now = get_current_timestamp();
        let token = |audience: serde_json::Value| {
            test_token_with_claims(
                json!({"exp": now + 3600, "iss": TEST_ISSUER, "aud": audience}),
                Some(TEST_KID),
            )
        };

        authenticator
            .authenticate(&token(json!("https://logs.example.com")))
            .await
            .unwrap();
        authenticator
            .authenticate(&token(json!(["https://logs.example.com"])))
            .await
            .unwrap();

        // A multi-audience token is accepted when one entry names us, per RFC 7519.
        authenticator
            .authenticate(&token(json!(["https://logs.example.com", "other-api"])))
            .await
            .unwrap();

        // A token minted for another audience, or with no audience at all, fails.
        assert!(
            authenticator
                .authenticate(&token(json!(["other-client"])))
                .await
                .is_err()
        );
        assert!(
            authenticator
                .authenticate(&token(json!("other-client")))
                .await
                .is_err()
        );
        let missing = test_token_with_claims(
            json!({"exp": now + 3600, "iss": TEST_ISSUER}),
            Some(TEST_KID),
        );
        assert!(authenticator.authenticate(&missing).await.is_err());
    }

    #[tokio::test]
    async fn kid_selection_rules_are_enforced() {
        let authenticator = test_authenticator().await;
        let unknown = test_token(get_current_timestamp() + 3600, Some("unknown"));
        assert!(authenticator.authenticate(&unknown).await.is_err());
        let no_kid = test_token(get_current_timestamp() + 3600, None);
        authenticator.authenticate(&no_kid).await.unwrap();

        let multiple = oidc_authenticator(
            &JwkSet {
                keys: vec![test_jwk(Some("one")), test_jwk(Some("two"))],
            },
            Duration::from_secs(3600),
            CACHE_IDLE_TIMEOUT,
        )
        .await;
        let second = test_token(get_current_timestamp() + 3600, Some("two"));
        multiple.authenticate(&second).await.unwrap();
        let no_kid = test_token(get_current_timestamp() + 3600, None);
        assert!(multiple.authenticate(&no_kid).await.is_err());
    }

    #[tokio::test]
    async fn invalid_jwks_keys_are_rejected_at_startup() {
        assert!(
            authenticator_with_jwks_value(json!({"keys": []}))
                .await
                .is_err()
        );
        assert!(
            authenticator_with_jwks_value(json!({
                "keys": [{"kty": "oct", "k": "c2VjcmV0", "alg": "HS256"}]
            }))
            .await
            .is_err()
        );

        let duplicate = JwkSet {
            keys: vec![test_jwk(Some("same")), test_jwk(Some("same"))],
        };
        assert!(
            authenticator_with_jwks_value(serde_json::to_value(&duplicate).unwrap())
                .await
                .is_err()
        );

        let mut short_key = test_jwk(Some(TEST_KID));
        let AlgorithmParameters::OctetKeyPair(parameters) = &mut short_key.algorithm else {
            panic!("expected Ed25519 key");
        };
        parameters.x = "AA".to_owned();
        assert!(
            authenticator_with_jwks_value(
                serde_json::to_value(&JwkSet {
                    keys: vec![short_key]
                })
                .unwrap()
            )
            .await
            .is_err()
        );

        let mut empty_kid = test_jwk(Some(TEST_KID));
        empty_kid.common.key_id = Some(String::new());
        assert!(
            authenticator_with_jwks_value(
                serde_json::to_value(&JwkSet {
                    keys: vec![empty_kid]
                })
                .unwrap()
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn oidc_issuer_loads_discovery_jwks_and_checks_iss() {
        let fetcher = Arc::new(MockFetcher::new(&[
            (TEST_DISCOVERY_URL, vec![test_discovery()]),
            (
                TEST_JWKS_URL,
                vec![serde_json::to_value(test_jwks()).unwrap()],
            ),
        ]));
        let authenticator = JwtAuthenticator::from_oidc_issuer_with_fetcher(
            TEST_ISSUER,
            &[TEST_AUDIENCE.to_owned()],
            &[],
            fetcher.clone(),
            Duration::from_secs(3600),
            CACHE_IDLE_TIMEOUT,
        )
        .await
        .unwrap();
        let now = get_current_timestamp();

        let valid = test_token_with_claims(
            json!({"exp": now + 3600, "iss": TEST_ISSUER, "aud": TEST_AUDIENCE}),
            Some(TEST_KID),
        );
        authenticator.authenticate(&valid).await.unwrap();

        for claims in [
            json!({"exp": now + 3600, "iss": "https://other.example.com", "aud": TEST_AUDIENCE}),
            json!({"exp": now + 3600, "aud": TEST_AUDIENCE}),
        ] {
            let token = test_token_with_claims(claims, Some(TEST_KID));
            assert!(authenticator.authenticate(&token).await.is_err());
        }
        assert_eq!(fetcher.calls_of(TEST_JWKS_URL), 1);
    }

    #[tokio::test]
    async fn oidc_discovery_issuer_mismatch_is_rejected() {
        let fetcher = Arc::new(MockFetcher::new(&[(
            TEST_DISCOVERY_URL,
            vec![json!({
                "issuer": "https://other.example.com",
                "jwks_uri": TEST_JWKS_URL,
            })],
        )]));
        let error = match JwtAuthenticator::from_oidc_issuer_with_fetcher(
            TEST_ISSUER,
            &[TEST_AUDIENCE.to_owned()],
            &[],
            fetcher,
            Duration::from_secs(3600),
            CACHE_IDLE_TIMEOUT,
        )
        .await
        {
            Ok(_) => panic!("issuer mismatch must be rejected"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("does not match"));
    }

    #[tokio::test]
    async fn oidc_requires_http_urls_for_issuer_and_jwks_uri() {
        for issuer in ["ftp://idp.example.com", "not a url"] {
            let fetcher = Arc::new(MockFetcher::default());
            assert!(
                JwtAuthenticator::from_oidc_issuer_with_fetcher(
                    issuer,
                    &[TEST_AUDIENCE.to_owned()],
                    &[],
                    fetcher,
                    Duration::from_secs(3600),
                    CACHE_IDLE_TIMEOUT,
                )
                .await
                .is_err(),
                "accepted issuer {issuer:?}"
            );
        }
        for jwks_uri in ["ftp://idp.example.com/jwks", "jwks.json"] {
            let fetcher = Arc::new(MockFetcher::new(&[(
                TEST_DISCOVERY_URL,
                vec![json!({"issuer": TEST_ISSUER, "jwks_uri": jwks_uri})],
            )]));
            assert!(
                JwtAuthenticator::from_oidc_issuer_with_fetcher(
                    TEST_ISSUER,
                    &[TEST_AUDIENCE.to_owned()],
                    &[],
                    fetcher,
                    Duration::from_secs(3600),
                    CACHE_IDLE_TIMEOUT,
                )
                .await
                .is_err(),
                "accepted jwks_uri {jwks_uri:?}"
            );
        }
    }

    #[tokio::test]
    async fn oidc_refreshes_jwks_on_unknown_kid() {
        let fetcher = Arc::new(MockFetcher::new(&[
            (TEST_DISCOVERY_URL, vec![test_discovery()]),
            (
                TEST_JWKS_URL,
                vec![
                    serde_json::to_value(standby_jwks()).unwrap(),
                    serde_json::to_value(test_jwks()).unwrap(),
                ],
            ),
        ]));
        let authenticator = JwtAuthenticator::from_oidc_issuer_with_fetcher(
            TEST_ISSUER,
            &[TEST_AUDIENCE.to_owned()],
            &[],
            fetcher.clone(),
            Duration::ZERO,
            CACHE_IDLE_TIMEOUT,
        )
        .await
        .unwrap();
        let token = test_token(get_current_timestamp() + 3600, Some(TEST_KID));

        authenticator.authenticate(&token).await.unwrap();
        assert_eq!(fetcher.calls_of(TEST_JWKS_URL), 2);
    }

    #[tokio::test]
    async fn oidc_refresh_is_rate_limited() {
        let fetcher = Arc::new(MockFetcher::new(&[
            (TEST_DISCOVERY_URL, vec![test_discovery()]),
            (
                TEST_JWKS_URL,
                vec![
                    serde_json::to_value(standby_jwks()).unwrap(),
                    serde_json::to_value(test_jwks()).unwrap(),
                ],
            ),
        ]));
        let authenticator = JwtAuthenticator::from_oidc_issuer_with_fetcher(
            TEST_ISSUER,
            &[TEST_AUDIENCE.to_owned()],
            &[],
            fetcher.clone(),
            Duration::from_secs(3600),
            CACHE_IDLE_TIMEOUT,
        )
        .await
        .unwrap();

        for kid in ["ghost-1", "ghost-2"] {
            let token = test_token(get_current_timestamp() + 3600, Some(kid));
            assert!(authenticator.authenticate(&token).await.is_err());
        }
        assert_eq!(fetcher.calls_of(TEST_JWKS_URL), 2);
    }

    #[test]
    fn bearer_header_distinguishes_missing_and_invalid_credentials() {
        let mut headers = HeaderMap::new();
        assert_eq!(
            bearer_token(&headers).unwrap_err().kind,
            AuthErrorKind::Missing
        );
        for value in ["Basic token", "Bearer", "Bearer "] {
            headers.insert(AUTHORIZATION, HeaderValue::from_str(value).unwrap());
            assert_eq!(
                bearer_token(&headers).unwrap_err().kind,
                AuthErrorKind::Missing,
                "unexpected error for {value:?}"
            );
        }
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer two tokens"));
        assert_eq!(
            bearer_token(&headers).unwrap_err().kind,
            AuthErrorKind::InvalidToken
        );
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("bearer valid-token"),
        );
        assert_eq!(bearer_token(&headers).unwrap(), "valid-token");
    }
}
