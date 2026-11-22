//! Who may do what.
//!
//! The allowlist is enforced by `firebase-authn-plugin`: a request that reaches an
//! authenticated route already belongs to a configured account. Durable runs, however,
//! are visible only to the subject that started them, and a release is a team effort:
//! anyone on the list must see and retry anyone's run. So the gear starts and controls
//! every run as one service identity, and records the human who pressed the button
//! in its own `publisher_runs` table.
use crate::config::PublisherConfig;
use crate::domain::models::Person;
use async_trait::async_trait;
use std::collections::HashMap;
use toolkit_durable_execution::{
    AuthorizationError, Authorizer, CallerAction, ConstraintPolicy, ServiceAction,
};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

/// Tenant of every run. Arbitrary, but fixed: stored in existing runs.
pub const TENANT: Uuid = Uuid::from_u128(0x6765_6172_732d_7075_626c_6973_6865_7201);
/// Subject that owns every run.
pub const SERVICE_SUBJECT: Uuid = Uuid::from_u128(0x6765_6172_732d_7075_626c_6973_6865_7202);

/// The service identity that owns and controls runs.
///
/// # Panics
/// Never: the builder only fails on missing required fields, all of which are set.
#[must_use]
pub fn service_context() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(SERVICE_SUBJECT)
        .subject_tenant_id(TENANT)
        .subject_type("service")
        .build()
        .expect("service security context is complete")
}

/// Grants the service identity its tenant; denies everyone else.
pub struct ServiceAuthorizer;

#[async_trait]
impl Authorizer for ServiceAuthorizer {
    async fn caller_scope(
        &self,
        ctx: &SecurityContext,
        _: CallerAction,
    ) -> Result<AccessScope, AuthorizationError> {
        if ctx.subject_id() == SERVICE_SUBJECT && ctx.subject_tenant_id() == TENANT {
            Ok(AccessScope::for_tenant(TENANT))
        } else {
            Err(AuthorizationError::denied())
        }
    }

    async fn service_scope(
        &self,
        action: ServiceAction,
    ) -> Result<AccessScope, AuthorizationError> {
        Ok(match action.constraints() {
            ConstraintPolicy::Required => AccessScope::for_tenant(TENANT),
            _ => AccessScope::allow_all(),
        })
    }
}

/// Maps authenticated subject ids back to configured accounts.
#[derive(Debug, Default)]
pub struct Directory {
    by_subject: HashMap<Uuid, Person>,
}

impl Directory {
    #[must_use]
    pub fn from_config(config: &PublisherConfig) -> Self {
        let by_subject = config
            .users
            .iter()
            .map(|user| {
                (
                    subject_id(
                        &config.subject_namespace,
                        &config.firebase_project_id,
                        &user.uid,
                    ),
                    Person {
                        uid: user.uid.clone(),
                        email: user.email.clone(),
                    },
                )
            })
            .collect();
        Self { by_subject }
    }

    /// The person behind an authenticated request, if configured here too.
    #[must_use]
    pub fn person(&self, ctx: &SecurityContext) -> Option<&Person> {
        self.by_subject.get(&ctx.subject_id())
    }
}

/// The subject id `firebase-authn-plugin` assigns to a uid.
#[must_use]
pub fn subject_id(namespace: &Uuid, project_id: &str, uid: &str) -> Uuid {
    Uuid::new_v5(
        namespace,
        format!("firebase://{project_id}/{uid}").as_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn only_the_service_identity_is_a_caller() {
        let authorizer = ServiceAuthorizer;
        assert!(
            authorizer
                .caller_scope(&service_context(), CallerAction::Get)
                .await
                .is_ok()
        );
        let user = SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(TENANT)
            .subject_type("user")
            .build()
            .unwrap();
        assert!(
            authorizer
                .caller_scope(&user, CallerAction::Get)
                .await
                .is_err()
        );
    }

    #[test]
    fn subject_ids_match_the_plugin_scheme() {
        let ns = Uuid::NAMESPACE_URL;
        assert_eq!(
            subject_id(&ns, "p", "u"),
            Uuid::new_v5(&ns, b"firebase://p/u")
        );
    }
}
