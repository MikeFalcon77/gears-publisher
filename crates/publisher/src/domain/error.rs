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
    /// Signed in, but not (yet) allowed in.
    #[error("{0}")]
    NoAccess(NoAccess),
    #[error("only an admin may do this")]
    AdminOnly,
    #[error("{0}")]
    TooMany(String),
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

/// Why a signed-in account may not use the publisher.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NoAccess {
    #[error("ask for access first")]
    NotRegistered,
    #[error("your access request is waiting for an admin")]
    Pending,
    #[error("an admin declined your access request")]
    Rejected,
}

pub type Result<T> = std::result::Result<T, DomainError>;
