use sqlx::PgPool;

use crate::config;

#[derive(Debug, Clone)]
pub struct AppState {
    pub config: config::Config,
    pub db: PgPool,
}

impl AppState {
    pub fn new(config: config::Config, db: PgPool) -> Self {
        Self { config, db }
    }
}
