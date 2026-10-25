//! The admin listener's routes (phase 4): the dashboard, its fragments and live feed, the admin JSON API, embedded
//! assets, Prometheus `/metrics` and `/healthz`.

pub mod api;
pub mod assets;
pub mod charts;
pub mod feed;
pub mod pages;
pub mod views;

use std::convert::Infallible;
use std::sync::Arc;

use askama::Template;
use axum::Router;
use axum::extract::{Form, Path, Query, State};
use axum::http::header::{CONTENT_TYPE, LOCATION};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use futures::Stream;
use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;

use crate::server::admin::Admin;
use crate::store::ListFilter;
use views::Window;

#[derive(Debug, Default, Deserialize)]
pub struct Params {
    pub window: Option<String>,
    pub app: Option<String>,
    pub outcome: Option<String>,
    pub before: Option<String>,
    pub limit: Option<usize>,
}

impl Params {
    fn window(&self) -> Window {
        Window::parse(self.window.as_deref())
    }

    /// The app filter, if one is set (an empty value means all apps).
    fn app(&self) -> Option<&str> {
        self.app.as_deref().filter(|a| !a.is_empty())
    }
}

fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

pub fn router(admin: Arc<Admin>) -> Router {
    Router::new()
        .route("/", get(overview))
        .route("/fragments/overview", get(overview_fragment))
        .route("/fragments/status", get(status))
        .route("/fragments/inflight", get(inflight))
        .route("/live", get(live))
        .route("/live/events", get(live_events))
        .route("/exchanges/{id}", get(exchange))
        .route("/apps", get(apps))
        .route("/cache", get(cache))
        .route("/cache/clear", post(cache_clear))
        .route("/assets/{*path}", get(assets::serve))
        .route("/metrics", get(prometheus))
        .route("/healthz", get(|| async { "ok" }))
        .nest("/api", api::router())
        .with_state(admin)
}

fn html(template: &impl Template) -> Response {
    match template.render() {
        Ok(body) => Html(body).into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, format!("couldn't render the page: {error}")).into_response(),
    }
}

async fn overview(State(admin): State<Arc<Admin>>, Query(params): Query<Params>) -> Response {
    let now = now_ms();
    let overview = views::overview(&admin.metrics, params.window(), params.app(), now);
    let body = pages::overview_body(&admin, &overview).render().unwrap_or_default();
    html(&pages::overview_page(&admin, &overview, body, now))
}

async fn overview_fragment(State(admin): State<Arc<Admin>>, Query(params): Query<Params>) -> Response {
    let overview = views::overview(&admin.metrics, params.window(), params.app(), now_ms());
    html(&pages::overview_body(&admin, &overview))
}

async fn status(State(admin): State<Arc<Admin>>) -> Response {
    let status = admin.upstream_status().await;
    html(&pages::StatusFragment {
        up: status.up,
        upstream: admin.gateway.upstream_name().to_string(),
        version: status.version.unwrap_or_default(),
        in_flight: admin.gateway.inflight(),
    })
}

async fn inflight(State(admin): State<Arc<Admin>>) -> Response {
    html(&pages::inflight(&admin))
}

async fn live(State(admin): State<Arc<Admin>>, Query(params): Query<Params>) -> Response {
    let app = params.app().map(str::to_string);
    let rows = match &admin.store {
        Some(store) => {
            let filter = ListFilter { app: app.clone(), limit: 50, ..ListFilter::default() };
            store.list(filter).await.unwrap_or_default()
        }
        None => Vec::new(),
    };
    let events = match &app {
        Some(app) => format!("/live/events?app={app}"),
        None => "/live/events".into(),
    };
    html(&pages::LivePage {
        title: "Live".into(),
        nav: "live",
        app: app.unwrap_or_default(),
        events,
        inflight: pages::inflight(&admin).render().unwrap_or_default(),
        rows: rows.iter().rev().map(pages::row_html).collect(),
    })
}

/// Server-sent events: one `exchange` event (a table row) per finished exchange.
async fn live_events(
    State(admin): State<Arc<Admin>>,
    Query(params): Query<Params>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let app = params.app().map(str::to_string);
    let rx = admin.feed.subscribe();
    let stream = futures::stream::unfold((rx, app), |(mut rx, app)| async move {
        loop {
            match rx.recv().await {
                Ok(row) if app.as_deref().is_none_or(|a| a == row.app) => {
                    let event = Event::default().event("exchange").data(pages::row_html(&row));
                    return Some((Ok(event), (rx, app)));
                }
                Ok(_) | Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn exchange(State(admin): State<Arc<Admin>>, Path(id): Path<String>) -> Response {
    let Some(store) = &admin.store else { return (StatusCode::NOT_FOUND, "no request log").into_response() };
    match store.get(id.clone()).await {
        Ok(Some((row, bodies))) => html(&pages::exchange_page(&row, bodies.as_ref())),
        Ok(None) => (StatusCode::NOT_FOUND, format!("no exchange {id}")).into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

async fn apps(State(admin): State<Arc<Admin>>, Query(params): Query<Params>) -> Response {
    html(&pages::apps_page(&admin, params.window(), now_ms()))
}

async fn cache(State(admin): State<Arc<Admin>>) -> Response {
    let (stats, top) = match &admin.store {
        Some(store) => {
            let now = now_ms();
            (
                store.cache_stats(now).await.unwrap_or_default(),
                store.cache_list(None, 20, now).await.unwrap_or_default(),
            )
        }
        None => (Vec::new(), Vec::new()),
    };
    html(&pages::cache_page(&admin, stats, top))
}

#[derive(Debug, Default, Deserialize)]
pub struct ClearForm {
    pub app: Option<String>,
}

/// Clears stored entries (one app or all) and everything in memory; other apps' entries reload from the store.
pub async fn clear(admin: &Admin, app: Option<String>) -> Result<usize, crate::store::StoreError> {
    admin.gateway.cache().clear_memory();
    match &admin.store {
        Some(store) => store.cache_clear(app.filter(|a| !a.is_empty())).await,
        None => Ok(0),
    }
}

async fn cache_clear(State(admin): State<Arc<Admin>>, headers: HeaderMap, Form(form): Form<ClearForm>) -> Response {
    if let Err(error) = clear(&admin, form.app).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
    }
    // htmx follows HX-Redirect; a plain form post follows the 303.
    if headers.contains_key("hx-request") {
        let mut response = StatusCode::OK.into_response();
        response.headers_mut().insert("hx-redirect", HeaderValue::from_static("/cache"));
        response
    } else {
        (StatusCode::SEE_OTHER, [(LOCATION, HeaderValue::from_static("/cache"))]).into_response()
    }
}

async fn prometheus(State(admin): State<Arc<Admin>>) -> impl IntoResponse {
    let body = admin.metrics.render(admin.gauges());
    ([(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")], body)
}
