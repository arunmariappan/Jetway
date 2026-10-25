//! The admin JSON API (phase 4.4): the same data the pages show.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Json, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::json;

use super::{ClearForm, Params, clear, now_ms, views};
use crate::server::admin::Admin;
use crate::store::ListFilter;

pub fn router() -> Router<Arc<Admin>> {
    Router::new()
        .route("/overview", get(overview))
        .route("/exchanges", get(exchanges))
        .route("/exchanges/{id}", get(exchange))
        .route("/apps", get(apps))
        .route("/cache", get(cache))
        .route("/cache/clear", post(cache_clear))
}

fn error(status: StatusCode, message: impl ToString) -> Response {
    (status, Json(json!({"error": message.to_string()}))).into_response()
}

async fn overview(State(admin): State<Arc<Admin>>, Query(params): Query<Params>) -> Response {
    let overview = views::overview(&admin.metrics, params.window(), params.app(), now_ms());
    let gauges = admin.gauges();
    Json(json!({
        "window": overview.window,
        "app": overview.app,
        "tiles": overview.tiles,
        "rows": overview.rows,
        "in_flight": gauges.in_flight,
        "memory_bytes": gauges.memory_bytes,
        "log_dropped": gauges.log_dropped,
        "cache_entries": gauges.cache_entries,
        "upstream": admin.upstream_status().await,
    }))
    .into_response()
}

async fn exchanges(State(admin): State<Arc<Admin>>, Query(params): Query<Params>) -> Response {
    let Some(store) = &admin.store else { return error(StatusCode::NOT_FOUND, "no request log") };
    let filter = ListFilter {
        app: params.app().map(str::to_string),
        outcome: params.outcome.clone().filter(|o| !o.is_empty()),
        before: params.before.clone(),
        after: None,
        limit: params.limit.unwrap_or(50).clamp(1, 1000),
    };
    match store.list(filter).await {
        Ok(rows) => Json(json!({"exchanges": rows})).into_response(),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn exchange(State(admin): State<Arc<Admin>>, Path(id): Path<String>) -> Response {
    let Some(store) = &admin.store else { return error(StatusCode::NOT_FOUND, "no request log") };
    match store.get(id.clone()).await {
        Ok(Some((row, bodies))) => {
            let bodies = bodies.map(|b| {
                json!({
                    "request": String::from_utf8_lossy(&b.request),
                    "response": String::from_utf8_lossy(&b.response),
                })
            });
            Json(json!({"exchange": row, "bodies": bodies})).into_response()
        }
        Ok(None) => error(StatusCode::NOT_FOUND, format!("no exchange {id}")),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn apps(State(admin): State<Arc<Admin>>, Query(params): Query<Params>) -> Response {
    Json(json!({"window": params.window(), "apps": views::apps(&admin.metrics, params.window(), now_ms())}))
        .into_response()
}

async fn cache(State(admin): State<Arc<Admin>>) -> Response {
    let gauges = admin.gauges();
    let now = now_ms();
    let (stats, top) = match &admin.store {
        Some(store) => (
            store.cache_stats(now).await.unwrap_or_default(),
            store.cache_list(None, 20, now).await.unwrap_or_default(),
        ),
        None => (Vec::new(), Vec::new()),
    };
    Json(json!({
        "memory": {"entries": gauges.cache_entries, "bytes": gauges.cache_bytes},
        "flights": gauges.flights,
        "stored": stats,
        "top": top,
    }))
    .into_response()
}

async fn cache_clear(State(admin): State<Arc<Admin>>, body: Option<Json<ClearForm>>) -> Response {
    let app = body.and_then(|Json(form)| form.app);
    match clear(&admin, app).await {
        Ok(deleted) => Json(json!({"deleted": deleted, "memory_cleared": true})).into_response(),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}
