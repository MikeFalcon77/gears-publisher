//! `application/problem+json` errors.
use crate::domain::error::{DomainError, NoAccess};
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
            DomainError::NoAccess(NoAccess::NotRegistered) => {
                (StatusCode::FORBIDDEN, "not_registered")
            }
            DomainError::NoAccess(NoAccess::Pending) => (StatusCode::FORBIDDEN, "pending_approval"),
            DomainError::NoAccess(NoAccess::Rejected) => (StatusCode::FORBIDDEN, "rejected"),
            DomainError::AdminOnly => (StatusCode::FORBIDDEN, "admin_only"),
            DomainError::TooMany(_) => (StatusCode::TOO_MANY_REQUESTS, "too_many"),
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
