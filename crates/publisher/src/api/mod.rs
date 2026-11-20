//! REST, SSE and the UI shell.
mod assets;
mod stream;

use crate::service::{RunDetail, RunSummary, Service, ServiceError};
use crate::store::LogLine;
use crate::workflow::{ReleaseInput, STEPS};
use axum::{
    Extension, Json, Router,
    extract::{Path, Query},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use toolkit::api::{OpenApiRegistry, operation_builder::OperationBuilder};
use toolkit_security::SecurityContext;
use uuid::Uuid;

pub struct AppState {
    pub service: Arc<Service>,
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

type App = Extension<Arc<AppState>>;
type Ctx = Extension<SecurityContext>;
type ApiResult<T> = Result<Json<T>, ApiError>;

pub fn routes(mut router: Router, openapi: &dyn OpenApiRegistry, app: Arc<AppState>) -> Router {
    macro_rules! endpoint {
        ($method:ident, $path:expr, $id:expr, $handler:expr) => {
            router = OperationBuilder::$method($path)
                .operation_id($id)
                .authenticated()
                .no_license_required()
                .handler($handler)
                .json_response(StatusCode::OK, "Success")
                .standard_errors(openapi)
                .register(router, openapi);
        };
    }
    endpoint!(get, "/publisher/v1/me", "publisher.me", me);
    endpoint!(get, "/publisher/v1/runs", "publisher.list_runs", list_runs);
    endpoint!(post, "/publisher/v1/runs", "publisher.start_run", start_run);
    endpoint!(get, "/publisher/v1/runs/{id}", "publisher.get_run", get_run);
    endpoint!(
        get,
        "/publisher/v1/runs/{id}/log",
        "publisher.run_log",
        run_log
    );
    endpoint!(
        post,
        "/publisher/v1/runs/{id}/cancel",
        "publisher.cancel_run",
        cancel_run
    );
    endpoint!(
        post,
        "/publisher/v1/runs/{id}/resume",
        "publisher.resume_run",
        resume_run
    );
    endpoint!(
        post,
        "/publisher/v1/runs/{id}/retry",
        "publisher.retry_run",
        retry_run
    );
    router = OperationBuilder::get("/publisher/v1/runs/{id}/stream")
        .operation_id("publisher.stream_run")
        .authenticated()
        .no_license_required()
        .handler(stream::stream_run)
        .sse_json::<LogLine>(openapi, "Log lines (`log`) and run updates (`run`)")
        .register(router, openapi);

    for (path, id) in [
        ("/", "publisher.ui_index"),
        ("/index.html", "publisher.ui_index_html"),
        ("/runs/{id}", "publisher.ui_run"),
    ] {
        router = OperationBuilder::get(path)
            .operation_id(id)
            .anonymous()
            .handler(assets::index)
            .html_response(StatusCode::OK, "UI shell")
            .register(router, openapi);
    }
    for (path, id) in [
        ("/assets/{*path}", "publisher.ui_assets"),
        ("/favicon.svg", "publisher.ui_favicon"),
    ] {
        router = OperationBuilder::get(path)
            .operation_id(id)
            .anonymous()
            .handler(assets::asset)
            .text_response(StatusCode::OK, "Static asset", "application/octet-stream")
            .register(router, openapi);
    }
    router
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

#[derive(Serialize)]
struct Me {
    uid: String,
    email: String,
    steps: &'static [&'static str],
}

async fn me(Extension(app): App, Extension(ctx): Ctx) -> ApiResult<Me> {
    let person = app.service.person(&ctx)?;
    Ok(Json(Me {
        uid: person.uid.clone(),
        email: person.email.clone(),
        steps: &STEPS,
    }))
}

async fn list_runs(Extension(app): App, Extension(ctx): Ctx) -> ApiResult<Vec<RunSummary>> {
    app.service.person(&ctx)?;
    Ok(Json(app.service.list().await?))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct StartRequest {
    dry_run: bool,
}

#[derive(Serialize)]
struct Started {
    id: Uuid,
}

async fn start_run(
    Extension(app): App,
    Extension(ctx): Ctx,
    body: Option<Json<StartRequest>>,
) -> Result<(StatusCode, Json<Started>), ApiError> {
    let person = app.service.person(&ctx)?.clone();
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let id = app
        .service
        .start(
            &person,
            ReleaseInput {
                dry_run: body.dry_run,
            },
        )
        .await?;
    Ok((StatusCode::CREATED, Json(Started { id })))
}

async fn get_run(
    Extension(app): App,
    Extension(ctx): Ctx,
    Path(id): Path<Uuid>,
) -> ApiResult<RunDetail> {
    app.service.person(&ctx)?;
    Ok(Json(app.service.get(id).await?))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LogQuery {
    after: i64,
    limit: Option<u64>,
}

async fn run_log(
    Extension(app): App,
    Extension(ctx): Ctx,
    Path(id): Path<Uuid>,
    Query(q): Query<LogQuery>,
) -> ApiResult<Vec<LogLine>> {
    app.service.person(&ctx)?;
    Ok(Json(
        app.service
            .log(id, q.after, q.limit.unwrap_or(2000))
            .await?,
    ))
}

#[derive(Serialize)]
struct Accepted {
    ok: bool,
}

async fn cancel_run(
    Extension(app): App,
    Extension(ctx): Ctx,
    Path(id): Path<Uuid>,
) -> ApiResult<Accepted> {
    let person = app.service.person(&ctx)?;
    app.service.cancel(person, id).await?;
    Ok(Json(Accepted { ok: true }))
}

async fn resume_run(
    Extension(app): App,
    Extension(ctx): Ctx,
    Path(id): Path<Uuid>,
) -> ApiResult<Accepted> {
    let person = app.service.person(&ctx)?;
    app.service.resume(person, id).await?;
    Ok(Json(Accepted { ok: true }))
}

async fn retry_run(
    Extension(app): App,
    Extension(ctx): Ctx,
    Path(id): Path<Uuid>,
) -> ApiResult<Accepted> {
    let person = app.service.person(&ctx)?;
    app.service.retry(person, id).await?;
    Ok(Json(Accepted { ok: true }))
}

/// `application/problem+json` errors.
#[derive(Debug)]
pub struct ApiError(ServiceError);

impl From<ServiceError> for ApiError {
    fn from(error: ServiceError) -> Self {
        Self(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code) = match &self.0 {
            ServiceError::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            ServiceError::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
            ServiceError::Forbidden => (StatusCode::FORBIDDEN, "not_configured"),
            ServiceError::Invalid(_) => (StatusCode::BAD_REQUEST, "invalid"),
            ServiceError::Starting => (StatusCode::SERVICE_UNAVAILABLE, "starting"),
            ServiceError::Unavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
            ServiceError::Internal(error) => {
                tracing::error!(error = %format!("{error:#}"), "request failed");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal")
            }
        };
        let detail = match &self.0 {
            ServiceError::Internal(_) => "internal error; see the server log".to_owned(),
            other => other.to_string(),
        };
        let body = serde_json::json!({
            "type": "about:blank",
            "title": detail,
            "status": status.as_u16(),
            "detail": detail,
            "code": code,
        });
        (
            status,
            [(header::CONTENT_TYPE, "application/problem+json")],
            body.to_string(),
        )
            .into_response()
    }
}
