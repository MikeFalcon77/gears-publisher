//! Plugin lifecycle and its scoped SDK implementation.
use crate::config::{PublisherAuthnConfig, PublisherAuthnInstanceProbe};
use crate::firebase::{FirebaseVerifier, Refusal};
use async_trait::async_trait;
use authn_resolver_sdk::{
    AuthNResolverError, AuthNResolverPluginClient, AuthNResolverPluginSpecV1, AuthenticationResult,
    ClientCredentialsRequest,
};
use secrecy::SecretString;
use std::sync::{Arc, OnceLock};
use toolkit::gts_publication::GtsPublicationInputs;
use toolkit::plugins::bind_plugin_instances;
use toolkit::{Gear, GearCtx};
use toolkit_gts::PluginV1;
use toolkit_security::SecurityContext;
use tracing::{debug, warn};
use types_registry_sdk::GtsId;
use uuid::Uuid;

/// The GTS suffix this plugin registers under the `AuthN` resolver's plugin spec.
pub const PLUGIN_SUFFIX: &str = "cf.publisher.firebase_authn.plugin.v1";

/// Registers one scoped [`AuthNResolverPluginClient`] after the runtime admits its
/// configured instance.
#[toolkit::gear(
    name = "publisher-authn-plugin",
    gts_publication(entities = Self::plugin_instance),
)]
#[derive(Default)]
pub struct PublisherAuthnPlugin {
    service: OnceLock<Arc<AuthnService>>,
}

impl PublisherAuthnPlugin {
    /// The publication's config-only document: the plugin instance, from vendor and
    /// priority alone.
    fn plugin_instance(
        inputs: &GtsPublicationInputs<'_>,
    ) -> anyhow::Result<Vec<(GtsId, serde_json::Value)>> {
        let probe: PublisherAuthnInstanceProbe = inputs.config_or_default()?;
        let (id, document) = PluginV1::<AuthNResolverPluginSpecV1>::build_registration(
            PLUGIN_SUFFIX,
            probe.vendor,
            probe.priority,
        )?;
        Ok(vec![(GtsId::try_new(id.as_ref())?, document)])
    }
}

#[async_trait]
impl Gear for PublisherAuthnPlugin {
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let manifest = ctx.gts_publication_manifest()?;
        let config: PublisherAuthnConfig = ctx.config_expanded_or_default()?;
        config.validate()?;
        let verifier = FirebaseVerifier::new(config.project_id.clone())?;
        let service = Arc::new(AuthnService { config, verifier });
        self.service
            .set(service.clone())
            .map_err(|_| anyhow::anyhow!("publisher authn plugin already initialized"))?;
        let api: Arc<dyn AuthNResolverPluginClient> = service;
        bind_plugin_instances::<AuthNResolverPluginSpecV1, _>(
            &ctx.client_hub(),
            manifest.documents(),
            &api,
        )
    }
}

struct AuthnService {
    config: PublisherAuthnConfig,
    verifier: FirebaseVerifier,
}

impl AuthnService {
    /// The subject id for a Firebase uid, the scheme `firebase-authn-plugin` uses, so
    /// subjects keep their ids across the switch: the same uid always maps to the
    /// same id, and the project is part of the name, so two projects never collide.
    fn subject_id(&self, uid: &str) -> Uuid {
        Uuid::new_v5(
            &self.config.subject_namespace,
            format!("firebase://{}/{}", self.config.project_id, uid).as_bytes(),
        )
    }
}

/// One answer per kind of refusal, and the reason only in the log.
fn refusal_to_error(refusal: &Refusal) -> AuthNResolverError {
    match refusal {
        Refusal::CryptoUnavailable(cause) => {
            warn!(%cause, "Firebase crypto backend unavailable; answering 503");
            AuthNResolverError::ServiceUnavailable("crypto backend unavailable".to_owned())
        }
        Refusal::KeysUnavailable(cause) => {
            warn!(%cause, "Firebase signing keys unavailable; answering 503");
            AuthNResolverError::ServiceUnavailable("Firebase signing keys unavailable".to_owned())
        }
        Refusal::NotAValidToken(reason) => {
            debug!(%reason, "Firebase token refused");
            AuthNResolverError::Unauthorized("not a valid Firebase ID token".to_owned())
        }
    }
}

#[async_trait]
impl AuthNResolverPluginClient for AuthnService {
    async fn authenticate(&self, token: &str) -> Result<AuthenticationResult, AuthNResolverError> {
        let uid = self
            .verifier
            .verify_identity(token)
            .await
            .map_err(|refusal| refusal_to_error(&refusal))?;
        let security_context = SecurityContext::builder()
            .subject_id(self.subject_id(&uid))
            .subject_tenant_id(self.config.tenant_id)
            .subject_type("user")
            .token_scopes(self.config.token_scopes.clone())
            .bearer_token(SecretString::from(token.to_owned()))
            .build()
            .map_err(|error| AuthNResolverError::Internal(error.to_string()))?;
        Ok(AuthenticationResult { security_context })
    }

    async fn exchange_client_credentials(
        &self,
        _: &ClientCredentialsRequest,
    ) -> Result<AuthenticationResult, AuthNResolverError> {
        Err(AuthNResolverError::TokenAcquisitionFailed(
            "Firebase user sign-in required".to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::firebase::tests::{PROJECT, signed_claims, signing_keys};

    fn service(verifier: FirebaseVerifier) -> AuthnService {
        AuthnService {
            config: PublisherAuthnConfig {
                project_id: PROJECT.to_owned(),
                tenant_id: Uuid::from_u128(7),
                ..Default::default()
            },
            verifier,
        }
    }

    #[test]
    fn local_crypto_failure_returns_service_unavailable() {
        let refusal = Refusal::CryptoUnavailable("backend failed".to_owned());
        assert!(matches!(
            refusal_to_error(&refusal),
            AuthNResolverError::ServiceUnavailable(_)
        ));
    }

    /// Any genuine token gets a context: the configured tenant and scopes, never
    /// the token's, and the subject id of the old plugin's scheme.
    #[tokio::test]
    async fn any_signed_identity_gets_the_configured_tenant_and_scopes() {
        let service = service(crate::firebase::with_test_keys(
            PROJECT.to_owned(),
            signing_keys(),
        ));
        for uid in ["account-1", "stranger"] {
            let token = signed_claims(
                serde_json::json!({"sub": uid, "tenant_id": Uuid::nil(), "scopes": ["*"], "email_verified": false}),
            );
            let context = service.authenticate(&token).await.unwrap().security_context;
            assert_eq!(context.subject_tenant_id(), Uuid::from_u128(7));
            assert_eq!(context.token_scopes(), &["publisher".to_owned()]);
            assert_eq!(
                context.subject_id(),
                Uuid::new_v5(
                    &Uuid::NAMESPACE_URL,
                    format!("firebase://{PROJECT}/{uid}").as_bytes()
                )
            );
        }
    }

    #[test]
    fn the_subject_id_is_stable_per_uid_and_distinct_across_uids() {
        let service = service(FirebaseVerifier::without_http(PROJECT.to_owned()));
        assert_eq!(service.subject_id("uid-a"), service.subject_id("uid-a"));
        assert_ne!(service.subject_id("uid-a"), service.subject_id("uid-b"));
    }

    /// A key outage is the platform's problem and answers 503; a bad token 401.
    #[tokio::test]
    async fn a_key_outage_is_503_and_a_refused_token_is_401() {
        let token = signed_claims(serde_json::json!({}));
        let outage = service(FirebaseVerifier::without_http(PROJECT.to_owned()));
        assert!(matches!(
            outage.authenticate(&token).await,
            Err(AuthNResolverError::ServiceUnavailable(_))
        ));
        let keyed = service(crate::firebase::with_test_keys(
            PROJECT.to_owned(),
            signing_keys(),
        ));
        assert!(matches!(
            keyed.authenticate("not-a-token").await,
            Err(AuthNResolverError::Unauthorized(_))
        ));
    }
}

#[cfg(test)]
#[path = "gear_tests.rs"]
mod gear_tests;
