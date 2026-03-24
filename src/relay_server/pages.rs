use super::AppState;
use super::auth::ensure_browser_http_auth;
use axum::{
    extract::State,
    http::HeaderMap,
    response::Html,
    response::{IntoResponse, Response},
};

pub(super) async fn healthz() -> &'static str {
    "ok"
}

pub(super) async fn debug_console(headers: HeaderMap, State(state): State<AppState>) -> Response {
    if let Err(response) = ensure_browser_http_auth(&state, &headers).await {
        return *response;
    }

    (
        [("cache-control", "no-cache")],
        Html(include_str!("../relay_console.html")),
    )
        .into_response()
}

pub(super) async fn web_manifest(headers: HeaderMap, State(state): State<AppState>) -> Response {
    if let Err(response) = ensure_browser_http_auth(&state, &headers).await {
        return *response;
    }

    (
        [
            ("content-type", "application/manifest+json; charset=utf-8"),
            ("cache-control", "no-cache"),
        ],
        include_str!("../pwa_manifest.webmanifest"),
    )
        .into_response()
}

pub(super) async fn service_worker(headers: HeaderMap, State(state): State<AppState>) -> Response {
    if let Err(response) = ensure_browser_http_auth(&state, &headers).await {
        return *response;
    }

    (
        [
            ("content-type", "application/javascript; charset=utf-8"),
            ("cache-control", "no-cache"),
            ("service-worker-allowed", "/"),
        ],
        include_str!("../pwa_service_worker.js"),
    )
        .into_response()
}

pub(super) async fn app_icon(headers: HeaderMap, State(state): State<AppState>) -> Response {
    if let Err(response) = ensure_browser_http_auth(&state, &headers).await {
        return *response;
    }

    (
        [
            ("content-type", "image/svg+xml"),
            ("cache-control", "no-cache"),
        ],
        include_str!("../pwa_icon.svg"),
    )
        .into_response()
}

pub(super) async fn app_maskable_icon(
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Response {
    if let Err(response) = ensure_browser_http_auth(&state, &headers).await {
        return *response;
    }

    (
        [
            ("content-type", "image/svg+xml"),
            ("cache-control", "no-cache"),
        ],
        include_str!("../pwa_icon_maskable.svg"),
    )
        .into_response()
}
