//! `application/problem+json` errors.
use crate::domain::error::DomainError;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

#[derive(Debug)]
pub struct ApiError(DomainError);

pub type ApiResult<T> = Result<axum::Json<T>, ApiError>;

impl From<DomainError> for ApiError {
    fn from(error: DomainError) -> Self {
        Self(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code) = match &self.0 {
            DomainError::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            DomainError::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
            DomainError::Forbidden => (StatusCode::FORBIDDEN, "not_configured"),
            DomainError::Invalid(_) => (StatusCode::BAD_REQUEST, "invalid"),
            DomainError::Starting => (StatusCode::SERVICE_UNAVAILABLE, "starting"),
            DomainError::Unavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
            DomainError::Internal(error) => {
                tracing::error!(error = %format!("{error:#}"), "request failed");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal")
            }
        };
        let detail = match &self.0 {
            DomainError::Internal(_) => "internal error; see the server log".to_owned(),
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
