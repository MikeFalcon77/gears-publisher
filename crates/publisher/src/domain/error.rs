//! Errors of the publisher's use cases; the REST layer maps them to problems.
use toolkit_durable_execution::ErrorKind;
use toolkit_macros::domain_model;

#[domain_model]
#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    #[error("not found")]
    NotFound,
    #[error("{0}")]
    Conflict(String),
    #[error("this account is not configured in the publisher")]
    Forbidden,
    #[error("{0}")]
    Invalid(String),
    #[error("the engine is starting, try again shortly")]
    Starting,
    #[error("{0}")]
    Unavailable(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<toolkit_durable_execution::Error> for DomainError {
    fn from(error: toolkit_durable_execution::Error) -> Self {
        let message = error.to_string();
        match error.kind() {
            ErrorKind::NotFound => Self::NotFound,
            ErrorKind::AlreadyExists | ErrorKind::FailedPrecondition | ErrorKind::Aborted => {
                Self::Conflict(message)
            }
            ErrorKind::InvalidArgument => Self::Invalid(message),
            ErrorKind::Unavailable => Self::Unavailable(message),
            _ => Self::Internal(anyhow::Error::new(error)),
        }
    }
}

pub type Result<T> = std::result::Result<T, DomainError>;
