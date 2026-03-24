use super::{AppState, BrowserLoginSession};
use crate::relay_state::unix_timestamp_secs;
use anyhow::{Context, Result, anyhow};
use axum::{
    extract::{Json, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::Html,
    response::Redirect,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use std::{collections::HashSet, net::SocketAddr};

#[derive(Debug, Clone)]
pub(super) struct LocalLoginAuthConfig {
    pub(super) username: String,
    pub(super) password: String,
    pub(super) session_ttl_secs: u64,
    pub(super) cookie_name: String,
    pub(super) cookie_secure: bool,
}

#[derive(Debug, Clone)]
pub(super) struct BrowserAuthConfig {
    pub(super) login: LocalLoginAuthConfig,
    pub(super) allowed_origins: HashSet<String>,
}

impl BrowserAuthConfig {
    pub(super) fn mode_name(&self) -> &'static str {
        "local_login"
    }
}

#[derive(Debug, Clone)]
pub(super) struct AuthenticatedBrowser {
    pub(super) user_id: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct LoginRequest {
    username: String,
    password: String,
}

pub(super) async fn login_page(headers: HeaderMap, State(state): State<AppState>) -> Response {
    if authenticate_browser_http_request(&state, &headers)
        .await
        .is_ok()
    {
        return Redirect::to("/").into_response();
    }

    (
        [("cache-control", "no-cache")],
        Html(include_str!("../relay_login.html")),
    )
        .into_response()
}

pub(super) async fn login_submit(
    headers: HeaderMap,
    State(state): State<AppState>,
    Json(payload): Json<LoginRequest>,
) -> Response {
    let config = &state.inner.browser_auth.login;

    if let Err(response) = validate_browser_origin(&state.inner.browser_auth, &headers) {
        return *response;
    }

    if payload.username != config.username || payload.password != config.password {
        return (StatusCode::UNAUTHORIZED, "invalid credentials").into_response();
    }

    let token = uuid::Uuid::new_v4().to_string();
    let expires_at = unix_timestamp_secs().saturating_add(config.session_ttl_secs);
    {
        let mut sessions = state.inner.login_sessions.lock().await;
        sessions.insert(
            token.clone(),
            BrowserLoginSession {
                user_id: config.username.clone(),
                expires_at,
            },
        );
    }

    let cookie = match build_auth_cookie(
        &config.cookie_name,
        &token,
        config.session_ttl_secs,
        config.cookie_secure,
    ) {
        Ok(cookie) => cookie,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to build auth cookie: {err}"),
            )
                .into_response();
        }
    };

    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .append(axum::http::header::SET_COOKIE, cookie);
    response
}

pub(super) async fn logout_submit(headers: HeaderMap, State(state): State<AppState>) -> Response {
    let config = &state.inner.browser_auth.login;
    if let Err(response) = validate_browser_origin(&state.inner.browser_auth, &headers) {
        return *response;
    }

    if let Some(session_token) = cookie_value(&headers, &config.cookie_name) {
        let mut sessions = state.inner.login_sessions.lock().await;
        sessions.remove(&session_token);
    }

    let clear_cookie = match build_expired_auth_cookie(&config.cookie_name, config.cookie_secure) {
        Ok(cookie) => cookie,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to clear auth cookie: {err}"),
            )
                .into_response();
        }
    };

    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .append(axum::http::header::SET_COOKIE, clear_cookie);
    response
}

pub(super) fn optional_nonempty_env(key: &str) -> Result<Option<String>> {
    match std::env::var(key) {
        Ok(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                Err(anyhow!("{key} 不能为空字符串"))
            } else {
                Ok(Some(trimmed.to_string()))
            }
        }
        Err(_) => Ok(None),
    }
}

pub(super) fn required_nonempty_env(key: &str) -> Result<String> {
    optional_nonempty_env(key)?.ok_or_else(|| anyhow!("缺少环境变量 {key}"))
}

pub(super) fn parse_origin_set_env(key: &str) -> Result<HashSet<String>> {
    let origins = parse_string_set_env(key)?;
    let mut normalized = HashSet::with_capacity(origins.len());
    for origin in origins {
        let normalized_origin =
            normalize_origin(&origin).ok_or_else(|| anyhow!("{key} 包含非法 origin: {origin}"))?;
        normalized.insert(normalized_origin);
    }
    Ok(normalized)
}

pub(super) fn validate_auth_configuration(
    bind_addr: SocketAddr,
    allow_insecure_dev: bool,
    accepted_client_tokens: &HashSet<String>,
    browser_auth: &BrowserAuthConfig,
) -> Result<()> {
    if allow_insecure_dev {
        if !bind_addr.ip().is_loopback() {
            return Err(anyhow!(
                "RELAY_SERVER_ALLOW_INSECURE_DEV=true 仅允许在 loopback 监听地址上使用"
            ));
        }
        return Ok(());
    }

    if accepted_client_tokens.is_empty() {
        return Err(anyhow!(
            "缺少 RELAY_SERVER_CLIENT_TOKENS；生产模式必须启用 home client token 认证"
        ));
    }
    if browser_auth.allowed_origins.is_empty() {
        return Err(anyhow!(
            "生产模式必须设置 RELAY_SERVER_BROWSER_ALLOWED_ORIGINS（逗号分隔）用于浏览器 Origin 校验"
        ));
    }

    let config = &browser_auth.login;
    if config.username.trim().is_empty() {
        return Err(anyhow!("RELAY_SERVER_LOGIN_USERNAME 不能为空"));
    }
    if config.password.trim().is_empty() {
        return Err(anyhow!("RELAY_SERVER_LOGIN_PASSWORD 不能为空"));
    }

    Ok(())
}

pub(super) fn parse_bool_env(key: &str, value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(anyhow!("{key} 必须是布尔值")),
    }
}

pub(super) async fn authenticate_browser_ws_request(
    state: &AppState,
    headers: &HeaderMap,
) -> std::result::Result<AuthenticatedBrowser, Box<Response>> {
    authenticate_browser_request(state, headers, true).await
}

pub(super) async fn ensure_browser_http_auth(
    state: &AppState,
    headers: &HeaderMap,
) -> std::result::Result<AuthenticatedBrowser, Box<Response>> {
    match authenticate_browser_http_request(state, headers).await {
        Ok(browser) => Ok(browser),
        Err(_) => Err(Box::new(Redirect::to("/login").into_response())),
    }
}

fn parse_string_set_env(key: &str) -> Result<HashSet<String>> {
    let Ok(raw) = std::env::var(key) else {
        return Ok(HashSet::new());
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(HashSet::new());
    }

    let values = trimmed
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect::<HashSet<_>>();

    if values.is_empty() {
        return Err(anyhow!("{key} 至少需要包含一个非空值"));
    }

    Ok(values)
}

fn normalize_origin(value: &str) -> Option<String> {
    let trimmed = value.trim().trim_end_matches('/');
    let (scheme, rest) = trimmed.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "https" && scheme != "http" {
        return None;
    }
    if rest.is_empty() || rest.contains('/') || rest.contains('?') || rest.contains('#') {
        return None;
    }
    Some(format!("{scheme}://{}", rest.to_ascii_lowercase()))
}

fn cookie_value(headers: &HeaderMap, cookie_name: &str) -> Option<String> {
    let cookie_header = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    for segment in cookie_header.split(';') {
        let part = segment.trim();
        if part.is_empty() {
            continue;
        }
        let (name, value) = part.split_once('=')?;
        if name.trim() == cookie_name {
            return Some(value.trim().to_string());
        }
    }
    None
}

fn validate_browser_origin(
    auth: &BrowserAuthConfig,
    headers: &HeaderMap,
) -> std::result::Result<(), Box<Response>> {
    if auth.allowed_origins.is_empty() {
        return Ok(());
    }

    let Some(origin_header) = header_value(headers, "Origin") else {
        return Err(unauthorized_response("browser origin header missing"));
    };
    let Some(origin) = normalize_origin(&origin_header) else {
        return Err(unauthorized_response("browser origin header invalid"));
    };
    if !auth.allowed_origins.contains(&origin) {
        return Err(unauthorized_response("browser origin not allowed"));
    }

    Ok(())
}

fn build_auth_cookie(
    cookie_name: &str,
    token: &str,
    max_age_secs: u64,
    secure: bool,
) -> Result<HeaderValue> {
    let secure_flag = if secure { "; Secure" } else { "" };
    let value = format!(
        "{cookie_name}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age_secs}{secure_flag}"
    );
    HeaderValue::from_str(&value).context("构建登录 cookie 失败")
}

fn build_expired_auth_cookie(cookie_name: &str, secure: bool) -> Result<HeaderValue> {
    let secure_flag = if secure { "; Secure" } else { "" };
    let value = format!("{cookie_name}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{secure_flag}");
    HeaderValue::from_str(&value).context("构建退出登录 cookie 失败")
}

async fn authenticate_browser_http_request(
    state: &AppState,
    headers: &HeaderMap,
) -> std::result::Result<AuthenticatedBrowser, Box<Response>> {
    authenticate_browser_request(state, headers, false).await
}

async fn authenticate_browser_request(
    state: &AppState,
    headers: &HeaderMap,
    enforce_origin: bool,
) -> std::result::Result<AuthenticatedBrowser, Box<Response>> {
    let auth = &state.inner.browser_auth;
    if enforce_origin {
        validate_browser_origin(auth, headers)?;
    }

    let config = &auth.login;
    let Some(session_token) = cookie_value(headers, &config.cookie_name) else {
        return Err(unauthorized_response("browser login session missing"));
    };

    let now = unix_timestamp_secs();
    let mut login_sessions = state.inner.login_sessions.lock().await;
    let Some(session) = login_sessions.get(&session_token).cloned() else {
        return Err(unauthorized_response("browser login session invalid"));
    };
    if session.expires_at <= now {
        login_sessions.remove(&session_token);
        return Err(unauthorized_response("browser login session expired"));
    }

    Ok(AuthenticatedBrowser {
        user_id: session.user_id,
    })
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn unauthorized_response(message: &str) -> Box<Response> {
    Box::new((StatusCode::UNAUTHORIZED, message.to_string()).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_origin_requires_scheme_and_host_only() {
        assert_eq!(
            normalize_origin("https://Relay.Example.com"),
            Some("https://relay.example.com".to_string())
        );
        assert_eq!(
            normalize_origin("https://relay.example.com/"),
            Some("https://relay.example.com".to_string())
        );
        assert!(normalize_origin("https://relay.example.com/path").is_none());
        assert!(normalize_origin("javascript:alert(1)").is_none());
    }
}
