//! trout: an OpenAI-compatible relay that cuts what a client on a metered link has to upload.

pub mod auth;
pub mod codec;
pub mod config;
pub mod dashboard;
pub mod error;
pub mod pages;
pub mod proxy;
pub mod responses;
pub mod store;
pub mod target;
pub mod telemetry;
pub mod upstream;

use std::sync::Arc;

use axum::response::{IntoResponse, Response};
use axum::routing::{any, delete, get, post};
use axum::{Json, Router, middleware};
use serde_json::json;

use crate::config::Config;
use crate::error::RelayError;
use crate::responses::handlers;
use crate::store::sweeper::{self, Sweeper};
use crate::store::{Store, StoreError};
use crate::telemetry::TrafficRecorder;
use crate::upstream::Upstream;

pub struct AppState {
    pub config: Config,
    pub upstream: Upstream,
    pub store: Store,
    pub traffic: TrafficRecorder,
    pub started: std::time::Instant,
    _sweeper: Sweeper,
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("cannot build the outbound client: {0}")]
    Client(#[from] reqwest::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Builds the whole service. Check order per request: route, relay credential, target, body,
/// upstream call. Must be called inside a Tokio runtime: it starts the store sweeper.
pub fn build_app(config: Config) -> Result<Router, BuildError> {
    let upstream = Upstream::new(&config.limits)?;
    let store = Store::open(&config.store.path)?;
    let sweeper = sweeper::spawn(store.clone(), &config.store);
    let traffic = TrafficRecorder::spawn(store.clone());
    let state = Arc::new(AppState {
        config,
        upstream,
        store,
        traffic,
        started: std::time::Instant::now(),
        _sweeper: sweeper,
    });

    Ok(Router::new()
        .route(auth::HEALTH_PATH, get(health))
        .route(pages::HOME_PATH, get(pages::home))
        .route(pages::DASHBOARD_PATH, get(pages::dashboard))
        .route(pages::PROVIDERS_PATH, get(pages::providers))
        .route("/stats", get(dashboard::stats))
        .route("/v1/responses", post(handlers::create))
        .route(
            "/v1/responses/{id}",
            get(handlers::retrieve).delete(handlers::delete_response),
        )
        .route("/v1/conversations/{id}", delete(handlers::delete_conversation))
        .route("/v1/{*rest}", any(proxy::forward))
        .fallback(route_not_found)
        .layer(middleware::from_fn_with_state(state.clone(), auth::require_key))
        .layer(middleware::from_fn_with_state(state.clone(), telemetry::record))
        .with_state(state))
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}

async fn route_not_found() -> Response {
    RelayError::RouteNotFound.into_response()
}
