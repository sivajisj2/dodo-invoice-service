use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::{generate_api_key, hash_api_key};

#[derive(Debug, Deserialize)]
pub struct CreateBusinessRequest {
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct CreateBusinessResponse {
    pub id: Uuid,
    pub name: String,
    pub api_key: String,
}

pub async fn create_business(
    pool: &PgPool,
    request: CreateBusinessRequest,
) -> Result<CreateBusinessResponse, sqlx::Error> {
    let business_id = Uuid::new_v4();
    let api_key = generate_api_key();
    let key_hash = hash_api_key(&api_key);

    // Store a short prefix to help identify keys without exposing them.
    let key_prefix = api_key.chars().take(19).collect::<String>();

    let mut tx = pool.begin().await?;

    let name = request.name.trim();

    sqlx::query("INSERT INTO businesses (id, name) VALUES ($1, $2)")
        .bind(business_id)
        .bind(name)
        .execute(&mut *tx)
        .await?;

    sqlx::query(
        r#"
        INSERT INTO api_keys (id, business_id, key_prefix, key_hash)
        VALUES ($1, $2, $3, $4)
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(business_id)
    .bind(key_prefix)
    .bind(key_hash)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(CreateBusinessResponse {
        id: business_id,
        name: name.to_owned(),
        api_key,
    })
}
