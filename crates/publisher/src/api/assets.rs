//! The built UI (`web/dist`), served without authentication.
use super::AppState;
use axum::{
    Extension,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use std::path::Path;
use std::sync::Arc;

pub async fn index(Extension(app): Extension<Arc<AppState>>) -> Response {
    serve(&app.web_root, "index.html").await
}

pub async fn asset(
    Extension(app): Extension<Arc<AppState>>,
    request: axum::extract::Request,
) -> Response {
    let path = request.uri().path().trim_start_matches('/');
    if !safe(path) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    serve(&app.web_root, path).await
}

fn safe(path: &str) -> bool {
    !path.is_empty()
        && path.split('/').all(|part| {
            !part.is_empty() && part != "." && part != ".." && !part.contains(['\\', '%'])
        })
}

fn mime(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript",
        Some("css") => "text/css",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("woff2") => "font/woff2",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    }
}

async fn serve(root: &Path, path: &str) -> Response {
    match tokio::fs::read(root.join(path)).await {
        Ok(bytes) => {
            // Vite fingerprints everything under assets/.
            let cache = if path.starts_with("assets/") {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            };
            (
                [
                    (header::CONTENT_TYPE, mime(path)),
                    (header::CACHE_CONTROL, cache),
                ],
                bytes,
            )
                .into_response()
        }
        Err(_) if path == "index.html" => (
            StatusCode::SERVICE_UNAVAILABLE,
            "The UI is not built: run `npm run build` in web/ or set publisher.web_root.",
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn traversal_is_refused() {
        assert!(super::safe("assets/index-abc.js"));
        assert!(!super::safe("assets/../secret"));
        assert!(!super::safe("assets/%2e%2e/x"));
        assert!(!super::safe("assets//x"));
    }
}
