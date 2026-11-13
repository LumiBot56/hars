//! `hars` — the homeassistant-rust server.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::get,
};
use ha_core::{Attributes, EntityId, Hass};
use serde_json::{Value, json};
use tracing_subscriber::EnvFilter;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let hass = Hass::new();
    let addr = std::env::var("HARS_BIND").unwrap_or_else(|_| "0.0.0.0:8123".into());

    let app = Router::new()
        .route(
            "/api/",
            get(|| async { Json(json!({ "message": "API running." })) }),
        )
        .route("/api/states", get(list_states))
        .route("/api/states/{entity_id}", get(get_state).post(post_state))
        .with_state(hass);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("hars listening on http://{addr}");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

async fn list_states(State(hass): State<Hass>) -> Json<Value> {
    Json(json!(hass.states().all()))
}

async fn get_state(
    State(hass): State<Hass>,
    Path(entity_id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let id = EntityId::parse(&entity_id).map_err(|_| StatusCode::BAD_REQUEST)?;
    hass.states()
        .get(&id)
        .map(|s| Json(json!(s)))
        .ok_or(StatusCode::NOT_FOUND)
}

/// Same shape as Home Assistant's `POST /api/states/<entity_id>`.
async fn post_state(
    State(hass): State<Hass>,
    Path(entity_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    let id = EntityId::parse(&entity_id).map_err(|_| StatusCode::BAD_REQUEST)?;
    let state = body
        .get("state")
        .and_then(Value::as_str)
        .ok_or(StatusCode::BAD_REQUEST)?;
    let attributes: Attributes = body
        .get("attributes")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    Ok(Json(json!(hass.set_state(id, state, attributes))))
}
