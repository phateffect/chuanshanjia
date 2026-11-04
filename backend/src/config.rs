use std::{
    borrow::Cow,
    collections::BTreeMap,
    env, fs,
    net::{IpAddr, Ipv4Addr},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use reqwest::Url;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub server: ServerConfig,
    #[serde(default)]
    pub storages: BTreeMap<String, StorageConfig>,
    #[serde(default)]
    pub services: BTreeMap<String, ServiceConfig>,
}

impl AppConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read server config {}", path.display()))?;
        let config: Self = toml::from_str(&contents)
            .with_context(|| format!("failed to parse server config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        let issuer = Url::parse(&self.server.oidc_issuer)
            .with_context(|| format!("invalid server.oidc_issuer {:?}", self.server.oidc_issuer))?;
        ensure!(
            matches!(issuer.scheme(), "http" | "https"),
            "server.oidc_issuer must use http or https"
        );
        ensure!(
            !self.server.oidc_audiences.is_empty(),
            "server.oidc_audiences must list at least one audience"
        );
        ensure!(
            self.server
                .oidc_audiences
                .iter()
                .all(|audience| !audience.trim().is_empty()),
            "server.oidc_audiences must not contain empty entries"
        );
        ensure!(
            self.server
                .oidc_required_scopes
                .iter()
                .all(|scope| !scope.trim().is_empty()),
            "server.oidc_required_scopes must not contain empty entries"
        );
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_server_host")]
    pub host: IpAddr,
    #[serde(default = "default_server_port")]
    pub port: u16,
    pub oidc_issuer: String,
    /// Acceptable `aud` values. For JWT access tokens this is the resource
    /// identifier (RFC 8707).
    pub oidc_audiences: Vec<String>,
    /// Scopes the token must carry, e.g. `["logs:read"]`. Empty disables the check.
    #[serde(default)]
    pub oidc_required_scopes: Vec<String>,
}

fn default_server_host() -> IpAddr {
    IpAddr::V4(Ipv4Addr::UNSPECIFIED)
}

fn default_server_port() -> u16 {
    3000
}

#[derive(Deserialize)]
#[serde(tag = "provider", rename_all = "kebab-case", deny_unknown_fields)]
pub enum StorageConfig {
    AliyunSls {
        endpoint: String,
        project: String,
        access_key_id: Credential,
        access_key_secret: Credential,
        security_token: Option<Credential>,
    },
    Elasticsearch {
        base_url: String,
    },
    VolcesTls {
        region: String,
        /// Defaults to the public endpoint `https://tls-{region}.volces.com`.
        endpoint: Option<String>,
        access_key_id: Credential,
        access_key_secret: Credential,
        security_token: Option<Credential>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub storage: String,
    pub index: String,
    #[serde(default)]
    pub filters: BTreeMap<String, Value>,
    pub keyword: Option<String>,
    #[serde(default)]
    pub json_fields: Vec<String>,
    #[serde(default)]
    pub ignores: Vec<String>,
    #[serde(default)]
    pub lookups: Vec<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
pub enum Credential {
    Value(String),
    Env(EnvCredential),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvCredential {
    pub env: String,
}

impl Credential {
    pub fn resolve(&self, field: &str) -> Result<Cow<'_, str>> {
        let value = match self {
            Self::Value(value) => Cow::Borrowed(value.as_str()),
            Self::Env(EnvCredential { env: name }) => {
                ensure!(
                    !name.is_empty(),
                    "{field} environment variable name is empty"
                );
                Cow::Owned(env::var(name).with_context(|| {
                    format!("failed to read {field} from environment variable {name}")
                })?)
            }
        };
        ensure!(!value.is_empty(), "{field} must not be empty");
        Ok(value)
    }
}

pub fn server_config_path() -> Result<PathBuf> {
    Ok(env::current_dir()
        .context("could not determine the current working directory")?
        .join("server.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_is_valid() {
        let config: AppConfig = toml::from_str(include_str!("../server.example.toml")).unwrap();
        config.validate().unwrap();

        assert_eq!(config.server.host, IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert_eq!(config.server.port, 3000);
        assert_eq!(config.server.oidc_issuer, "https://oidc.example.com");
        assert_eq!(config.server.oidc_audiences, ["https://logs.example.com"]);
        assert_eq!(config.storages.len(), 1);
        let StorageConfig::Elasticsearch { base_url } = &config.storages["es"] else {
            panic!("example should enable the Elasticsearch storage");
        };
        assert_eq!(base_url, "http://127.0.0.1:9200");

        let foo = &config.services["foo"];
        assert_eq!(config.services.len(), 1);
        assert_eq!(foo.storage, "es");
        assert_eq!(foo.index, "app-logs");
        assert!(foo.filters.is_empty());
        assert!(foo.keyword.is_none());
        assert!(foo.json_fields.is_empty());
        assert!(foo.ignores.is_empty());
        assert!(foo.lookups.is_empty());
    }

    #[test]
    fn server_section_and_oidc_issuer_are_required() {
        let Err(server_error) = toml::from_str::<AppConfig>("") else {
            panic!("server section must be required");
        };
        assert!(server_error.to_string().contains("missing field `server`"));

        let Err(issuer_error) = toml::from_str::<AppConfig>("[server]\n") else {
            panic!("server.oidc_issuer must be required");
        };
        assert!(
            issuer_error
                .to_string()
                .contains("missing field `oidc_issuer`")
        );
    }

    #[test]
    fn jwks_field_is_rejected() {
        let Err(error) = toml::from_str::<AppConfig>(
            r#"
            [server]
            jwks = "jwks.json"
            oidc_issuer = "https://oidc.example.com"
            oidc_audiences = ["test-audience"]
            "#,
        ) else {
            panic!("server.jwks must be rejected");
        };

        assert!(error.to_string().contains("unknown field `jwks`"));
    }

    #[test]
    fn oidc_issuer_must_be_an_absolute_http_url() {
        for issuer in ["not a url", "oidc.example.com", "ftp://oidc.example.com"] {
            let config: AppConfig = toml::from_str(&format!(
                "[server]\noidc_issuer = \"{issuer}\"\noidc_audiences = [\"a\"]\n"
            ))
            .unwrap();
            assert!(config.validate().is_err(), "accepted {issuer:?}");
        }

        let config: AppConfig = toml::from_str(
            "[server]\noidc_issuer = \"https://oidc.example.com\"\noidc_audiences = [\"a\"]\n",
        )
        .unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn oidc_audiences_must_not_be_empty() {
        for audiences in ["[]", "[\"\"]"] {
            let config: AppConfig = toml::from_str(&format!(
                "[server]\noidc_issuer = \"https://idp.example.com\"\noidc_audiences = {audiences}\n"
            ))
            .unwrap();
            assert!(config.validate().is_err(), "accepted {audiences}");
        }
    }

    #[test]
    fn server_host_and_port_use_defaults() {
        let config: AppConfig = toml::from_str(
            r#"
            [server]
            oidc_issuer = "https://idp.example.com"
            oidc_audiences = ["test-audience"]
            "#,
        )
        .unwrap();

        assert_eq!(config.server.host, IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert_eq!(config.server.port, 3000);
        assert!(config.storages.is_empty());
        assert!(config.services.is_empty());
    }

    #[test]
    fn server_host_and_port_can_be_customized() {
        let config: AppConfig = toml::from_str(
            r#"
            [server]
            host = "127.0.0.1"
            port = 3001
            oidc_issuer = "https://idp.example.com"
            oidc_audiences = ["test-audience"]
            "#,
        )
        .unwrap();

        assert_eq!(config.server.host, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(config.server.port, 3001);
    }

    #[test]
    fn service_json_fields_are_supported() {
        let config: AppConfig = toml::from_str(
            r#"
            [server]
            oidc_issuer = "https://idp.example.com"
            oidc_audiences = ["test-audience"]

            [services.foo]
            storage = "es"
            index = "app-logs"
            json_fields = ["attribute", "resource"]
            "#,
        )
        .unwrap();

        assert_eq!(
            config.services["foo"].json_fields,
            ["attribute", "resource"]
        );
    }

    #[test]
    fn server_config_is_read_from_current_directory() {
        assert_eq!(
            server_config_path().unwrap(),
            env::current_dir().unwrap().join("server.toml")
        );
    }

    #[test]
    fn listen_is_rejected() {
        let Err(error) = toml::from_str::<AppConfig>(
            r#"
            [server]
            oidc_issuer = "https://idp.example.com"
            oidc_audiences = ["test-audience"]
            listen = "127.0.0.1:3000"
            "#,
        ) else {
            panic!("server.listen must be rejected");
        };

        assert!(error.to_string().contains("unknown field `listen`"));
    }

    #[test]
    fn literal_credentials_are_supported() {
        let config: AppConfig = toml::from_str(
            r#"
            [server]
            oidc_issuer = "https://idp.example.com"
            oidc_audiences = ["test-audience"]

            [storages.test]
            provider = "aliyun-sls"
            endpoint = "cn-hangzhou.log.aliyuncs.com"
            project = "test-project"
            access_key_id = "test-id"
            access_key_secret = "test-secret"
            "#,
        )
        .unwrap();

        let StorageConfig::AliyunSls {
            access_key_id,
            access_key_secret,
            security_token,
            ..
        } = &config.storages["test"]
        else {
            panic!("expected SLS storage");
        };
        assert_eq!(access_key_id.resolve("test.id").unwrap(), "test-id");
        assert_eq!(
            access_key_secret.resolve("test.secret").unwrap(),
            "test-secret"
        );
        assert!(security_token.is_none());
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let Err(error) = toml::from_str::<AppConfig>(
            r#"
            [server]
            oidc_issuer = "https://idp.example.com"
            oidc_audiences = ["test-audience"]

            [storages.test]
            provider = "elasticsearch"
            base_url = "http://127.0.0.1:9200"
            project = "unexpected"
            "#,
        ) else {
            panic!("unknown storage fields must be rejected");
        };

        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn volces_tls_storage_is_supported() {
        let config: AppConfig = toml::from_str(
            r#"
            [server]
            oidc_issuer = "https://idp.example.com"
            oidc_audiences = ["test-audience"]

            [storages.test]
            provider = "volces-tls"
            region = "cn-beijing"
            endpoint = "https://tls-cn-beijing.ivolces.com"
            access_key_id = "test-id"
            access_key_secret = { env = "TLS_TEST_ACCESS_KEY_SECRET" }
            "#,
        )
        .unwrap();

        let StorageConfig::VolcesTls {
            region,
            endpoint,
            access_key_id,
            access_key_secret,
            security_token,
        } = &config.storages["test"]
        else {
            panic!("expected Volces TLS storage");
        };
        assert_eq!(region, "cn-beijing");
        assert_eq!(
            endpoint.as_deref(),
            Some("https://tls-cn-beijing.ivolces.com")
        );
        assert_eq!(access_key_id.resolve("test.id").unwrap(), "test-id");
        assert_eq!(
            access_key_secret
                .resolve("test.secret")
                .unwrap_err()
                .to_string(),
            "failed to read test.secret from environment variable TLS_TEST_ACCESS_KEY_SECRET"
        );
        assert!(security_token.is_none());
    }

    #[test]
    fn volces_tls_topic_is_not_a_storage_field() {
        let Err(error) = toml::from_str::<AppConfig>(
            r#"
            [server]
            oidc_issuer = "https://idp.example.com"
            oidc_audiences = ["test-audience"]

            [storages.test]
            provider = "volces-tls"
            region = "cn-beijing"
            access_key_id = "test-id"
            access_key_secret = "test-secret"
            topic_id = "unexpected"
            "#,
        ) else {
            panic!("topic_id must be rejected; the TLS topic belongs to service.index");
        };

        assert!(error.to_string().contains("unknown field `topic_id`"));
    }

    #[test]
    fn unsupported_providers_are_rejected() {
        let Err(error) = toml::from_str::<AppConfig>(
            r#"
            [server]
            oidc_issuer = "https://idp.example.com"
            oidc_audiences = ["test-audience"]

            [storages.test]
            provider = "opensearch"
            "#,
        ) else {
            panic!("unsupported providers must be rejected");
        };

        assert!(error.to_string().contains("unknown variant"));
    }
}
