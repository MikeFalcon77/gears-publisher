//! REST, SSE and the UI shell.
mod assets;
pub mod dto;
pub mod error;
mod handlers;
mod routes;
mod sse;

use crate::domain::service::RunService;
use axum::{
    Extension, Router,
    http::header,
    middleware::{self, Next},
    response::Response,
};
use std::path::PathBuf;
use std::sync::Arc;
use toolkit::api::OpenApiRegistry;

pub struct AppState {
    pub service: Arc<RunService>,
    /// `owner/name` of the repository, for links.
    pub repository: String,
    pub web_root: PathBuf,
    /// `Content-Security-Policy` of the UI.
    pub csp: header::HeaderValue,
}

/// The UI loads only its own files. Firebase sign-in (popup) needs Google's script
/// loader, the auth iframe on `<project>.firebaseapp.com` and the token endpoints.
#[must_use]
pub fn content_security_policy(firebase_project_id: &str) -> header::HeaderValue {
    let auth_domain = format!("https://{firebase_project_id}.firebaseapp.com");
    let policy = format!(
        "default-src 'self'; \
         script-src 'self' https://apis.google.com; \
         connect-src 'self' https://identitytoolkit.googleapis.com \
         https://securetoken.googleapis.com https://www.googleapis.com; \
         frame-src {auth_domain} https://accounts.google.com; \
         img-src 'self' data: https://*.googleusercontent.com; \
         style-src 'self' 'unsafe-inline'; \
         object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    );
    header::HeaderValue::from_str(&policy)
        .unwrap_or_else(|_| header::HeaderValue::from_static("default-src 'self'"))
}

/// Registers every operation and the security headers.
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    app: Arc<AppState>,
) -> Router {
    routes::register(router, openapi)
        .layer(middleware::from_fn_with_state(
            Arc::clone(&app),
            security_headers,
        ))
        .layer(Extension(app))
}

async fn security_headers(
    axum::extract::State(app): axum::extract::State<Arc<AppState>>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let api = request.uri().path().starts_with("/publisher/");
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_SECURITY_POLICY, app.csp.clone());
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    headers.insert(header::X_FRAME_OPTIONS, "DENY".parse().unwrap());
    headers.insert(header::REFERRER_POLICY, "no-referrer".parse().unwrap());
    if api {
        headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    }
    response
}
