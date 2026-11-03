use std::{net::SocketAddr, sync::Arc};

use anyhow::Context;
use chuanshanjia_backend::{
    ResourceMetadata, app_full,
    auth::JwtAuthenticator,
    config::{AppConfig, server_config_path},
    provider::service::ServiceProvider,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config_path = server_config_path()?;
    let config = AppConfig::load(&config_path)?;
    let listen = SocketAddr::new(config.server.host, config.server.port);
    let authenticator = Arc::new(
        JwtAuthenticator::from_oidc_issuer(
            &config.server.oidc_issuer,
            &config.server.oidc_audiences,
            &config.server.oidc_required_scopes,
        )
        .await?,
    );
    let metadata = ResourceMetadata {
        // The first configured audience is the resource identifier this backend
        // answers for; it is what clients request via RFC 8707.
        resource: config.server.oidc_audiences[0].clone(),
        authorization_servers: vec![config.server.oidc_issuer.clone()],
        scopes_supported: config.server.oidc_required_scopes.clone(),
    };
    let provider = ServiceProvider::from_config(config)?;
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("failed to bind {listen}"))?;
    println!("listening on http://{}", listener.local_addr()?);
    axum::serve(
        listener,
        app_full(Arc::new(provider), Some(authenticator), Some(metadata)),
    )
    .await?;
    Ok(())
}
