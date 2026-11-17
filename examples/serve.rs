//! Runs the portal without the desktop app, for development and measurements.
//!   cargo run --release --example serve -- <data folder> [bind address] [admin bridge address]
//! Uses the data folder's settings (portal.env) and database. With a third
//! argument, the admin API is also reachable at that address for test scripts
//! (development only: the app itself never serves it over the network).

use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "access_portal=info".into()),
        )
        .init();
    let mut args = std::env::args().skip(1);
    let dir = std::path::PathBuf::from(args.next().unwrap_or_else(|| r"C:\ProgramData\AccessPortal".into()));
    if let Some(bind) = args.next() {
        std::env::set_var("BIND_ADDR", bind);
    }
    let bridge = args.next();
    let portal = Arc::new(access_portal::start(&dir).await?);
    println!("portal on http://{}", portal.address);

    if let Some(addr) = bridge {
        // POST/PUT/DELETE/GET <path> with an optional JSON body -> the in-process admin API.
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        println!("admin bridge on http://{addr} (development only)");
        let p = portal.clone();
        let app = axum::Router::new().fallback(move |req: axum::extract::Request| {
            let p = p.clone();
            async move {
                let method = req.method().to_string();
                // Keep the query (e.g. ?force=1): the app's own requests carry it too.
                let path = req.uri().path_and_query().map_or_else(|| req.uri().path().to_string(), |p| p.to_string());
                let bytes = axum::body::to_bytes(req.into_body(), 1 << 20).await.unwrap_or_default();
                let body = serde_json::from_slice(&bytes).ok();
                let (status, value) = p.admin(&method, &path, body).await;
                (axum::http::StatusCode::from_u16(status).unwrap_or_default(), axum::Json(value))
            }
        });
        tokio::spawn(async move { axum::serve(listener, app).await });
    }
    tokio::signal::ctrl_c().await?;
    Ok(())
}
