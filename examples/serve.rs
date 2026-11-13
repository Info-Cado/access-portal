//! Runs the portal without the desktop app, for development and measurements.
//!   cargo run --release --example serve -- <data folder> [bind address]
//! Uses the data folder's settings (portal.env) and database.

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
    let portal = access_portal::start(&dir).await?;
    println!("portal on http://{}", portal.address);
    tokio::signal::ctrl_c().await?;
    Ok(())
}
