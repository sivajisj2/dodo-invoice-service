use dodo_invoice_service::{config::Config, db, http, state::AppState};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env()?;

    let database_url =
        std::env::var("DATABASE_URL").map_err(|_| anyhow::anyhow!("DATABASE_URL is required"))?;

    let db = db::create_pool(&database_url).await?;

    // Apply pending migrations before accepting requests.
    sqlx::migrate!("./migrations").run(&db).await?;

    let state = AppState::new(config.clone(), db);

    // Start the webhook delivery worker in the background.
    tokio::spawn(dodo_invoice_service::webhooks::run_worker(state.clone()));

    let listener = tokio::net::TcpListener::bind(&config.app_address).await?;

    info!(
        address = %config.app_address,
        "invoice service started"
    );

    axum::serve(listener, http::router(state)).await?;

    Ok(())
}
