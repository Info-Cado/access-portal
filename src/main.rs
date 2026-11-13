mod auth;
mod config;
mod db;
mod error;
mod models;
mod rdp;
mod tiles;
mod transport;
mod video;
mod web;

use std::sync::Arc;

use std::str::FromStr;

use anyhow::Context;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tower_http::trace::TraceLayer;

/// Shared state handed to every handler.
#[derive(Clone)]
pub struct AppState {
    pub pool: sqlx::SqlitePool,
    pub config: Arc<config::Config>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "access_portal=info,tower_http=info".into()),
        )
        .init();

    let config = config::Config::from_env()?;

    // Enforce foreign keys on every connection (SQLite is off by default) and
    // create the database file if it doesn't exist.
    let connect_opts = SqliteConnectOptions::from_str(&config.database_url)
        .with_context(|| format!("parsing DATABASE_URL {}", config.database_url))?
        .create_if_missing(true)
        .foreign_keys(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(connect_opts)
        .await
        .with_context(|| format!("opening database {}", config.database_url))?;

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("running migrations")?;

    db::seed_admin(&pool, &config).await?;

    let bind_addr = config.bind_addr.clone();
    let state = AppState {
        pool,
        config: Arc::new(config),
    };

    let app = web::router(state).layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("binding {bind_addr}"))?;
    tracing::info!("access-portal listening on http://{bind_addr}");

    axum::serve(listener, app).await?;
    Ok(())
}
