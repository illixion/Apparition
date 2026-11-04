//! The HTTP surface. Phase 0 serves only the service protocol's capabilities; the route table in
//! docs/RUST_STORE.md is the parity checklist for the rest.

use axum::{Json, Router, routing::get};
use serde_json::{Value, json};

pub const PREFIX: &str = "/api/service/v1";

pub fn router() -> Router {
    Router::new().route(&format!("{PREFIX}/capabilities"), get(capabilities))
}

async fn capabilities() -> Json<Value> {
    Json(json!({
        "protocol": "apparition-service",
        "version": 1,
        "hostPlatform": std::env::consts::OS,
        "capabilities": [],
    }))
}
