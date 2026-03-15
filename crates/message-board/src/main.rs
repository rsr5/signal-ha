use std::sync::Arc;

use message_board::db;
use message_board::routes;

use axum::Router;
use tokio::net::TcpListener;
use tracing::info;

const DEFAULT_PORT: u16 = 9200;
const DEFAULT_DB_PATH: &str = "board.db";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let db_path = std::env::var("BOARD_DB_PATH").unwrap_or_else(|_| DEFAULT_DB_PATH.into());
    let port: u16 = std::env::var("BOARD_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT);

    let pool = db::Pool::open(&db_path)?;
    pool.migrate()?;
    info!(db = %db_path, port, "Board starting");

    let state = Arc::new(pool);
    let app = Router::new()
        .merge(routes::router())
        .with_state(state);

    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    info!(port, "Listening");
    axum::serve(listener, app).await?;

    Ok(())
}
