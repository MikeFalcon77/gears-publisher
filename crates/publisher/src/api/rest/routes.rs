//! Operations: the JSON API under `/publisher/v1` and the UI's pages and assets.
use super::{assets, dto::LogLineDto, handlers, sse};
use axum::Router;
use axum::http::StatusCode;
use toolkit::api::{OpenApiRegistry, operation_builder::OperationBuilder};

pub fn register(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
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
    endpoint!(get, "/publisher/v1/me", "publisher.me", handlers::me);
    endpoint!(
        get,
        "/publisher/v1/runs",
        "publisher.list_runs",
        handlers::list_runs
    );
    endpoint!(
        post,
        "/publisher/v1/runs",
        "publisher.start_run",
        handlers::start_release
    );
    endpoint!(
        post,
        "/publisher/v1/ci",
        "publisher.start_ci",
        handlers::start_ci
    );
    endpoint!(
        get,
        "/publisher/v1/ci/queue",
        "publisher.ci_queue",
        handlers::ci_queue
    );
    endpoint!(
        post,
        "/publisher/v1/ci/queue/{id}/cancel",
        "publisher.cancel_queued",
        handlers::cancel_queued
    );
    endpoint!(
        get,
        "/publisher/v1/runs/{id}",
        "publisher.get_run",
        handlers::get_run
    );
    endpoint!(
        get,
        "/publisher/v1/runs/{id}/log",
        "publisher.run_log",
        handlers::run_log
    );
    endpoint!(
        post,
        "/publisher/v1/runs/{id}/cancel",
        "publisher.cancel_run",
        handlers::cancel_run
    );
    endpoint!(
        post,
        "/publisher/v1/runs/{id}/resume",
        "publisher.resume_run",
        handlers::resume_run
    );
    endpoint!(
        post,
        "/publisher/v1/runs/{id}/retry",
        "publisher.retry_run",
        handlers::retry_run
    );
    endpoint!(
        post,
        "/publisher/v1/access-request",
        "publisher.request_access",
        handlers::request_access
    );
    endpoint!(
        get,
        "/publisher/v1/admin/users",
        "publisher.list_access",
        handlers::list_access
    );
    endpoint!(
        post,
        "/publisher/v1/admin/users/{subject}/{action}",
        "publisher.decide_access",
        handlers::decide_access
    );
    router = OperationBuilder::get("/publisher/v1/runs/{id}/stream")
        .operation_id("publisher.stream_run")
        .authenticated()
        .no_license_required()
        .handler(sse::stream_run)
        .sse_json::<LogLineDto>(openapi, "Log lines (`log`) and run updates (`run`)")
        .register(router, openapi);

    register_ui(router, openapi)
}

/// The UI shell for each page, and its static files.
fn register_ui(mut router: Router, openapi: &dyn OpenApiRegistry) -> Router {
    for (path, id) in [
        ("/", "publisher.ui_index"),
        ("/index.html", "publisher.ui_index_html"),
        ("/runs/{id}", "publisher.ui_run"),
        ("/ci", "publisher.ui_ci"),
        ("/admin", "publisher.ui_admin"),
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
}
