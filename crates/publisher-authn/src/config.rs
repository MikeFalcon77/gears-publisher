//! Plugin configuration. The tenant and scopes are deployment data, never the token's.
use serde::Deserialize;
use uuid::Uuid;

/// The plugin's configuration.
#[derive(Debug, Clone, Deserialize, toolkit_macros::ExpandVars)]
#[serde(default, deny_unknown_fields)]
pub struct PublisherAuthnConfig {
    /// Vendor this plugin instance registers under.
    pub vendor: String,
    /// Selection priority among `AuthN` resolver plugins; lower wins.
    pub priority: i16,
    /// The Firebase project: the expected audience, and part of the issuer.
    #[expand_vars]
    pub project_id: String,
    /// `UUIDv5` namespace for subject ids derived from Firebase uids.
    pub subject_namespace: Uuid,
    /// The tenant every subject acts in.
    pub tenant_id: Uuid,
    /// The scopes every subject's requests carry.
    pub token_scopes: Vec<String>,
}

impl Default for PublisherAuthnConfig {
    fn default() -> Self {
        Self {
            vendor: "constructorfabric".to_owned(),
            priority: 25,
            project_id: String::new(),
            subject_namespace: Uuid::NAMESPACE_URL,
            tenant_id: Uuid::nil(),
            token_scopes: vec!["publisher".to_owned()],
        }
    }
}

/// The GTS publication's view of [`PublisherAuthnConfig`]: vendor and priority only,
/// so a process without the server's environment can build the manifest.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct PublisherAuthnInstanceProbe {
    pub vendor: String,
    pub priority: i16,
}

impl Default for PublisherAuthnInstanceProbe {
    fn default() -> Self {
        let PublisherAuthnConfig {
            vendor, priority, ..
        } = PublisherAuthnConfig::default();
        Self { vendor, priority }
    }
}

impl PublisherAuthnConfig {
    /// # Errors
    /// Refuses a malformed project, a nil namespace or tenant, and missing scopes.
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.project_id.is_empty()
                && self.project_id.len() <= 63
                && self
                    .project_id
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-'),
            "invalid Firebase project ID"
        );
        anyhow::ensure!(
            !self.subject_namespace.is_nil(),
            "subject namespace must not be nil"
        );
        anyhow::ensure!(!self.tenant_id.is_nil(), "tenant_id must not be nil");
        anyhow::ensure!(
            !self.token_scopes.is_empty()
                && self
                    .token_scopes
                    .iter()
                    .all(|scope| !scope.trim().is_empty()),
            "token scopes are required"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> PublisherAuthnConfig {
        PublisherAuthnConfig {
            project_id: "firebase-authn-test".to_owned(),
            tenant_id: Uuid::from_u128(1),
            ..Default::default()
        }
    }

    #[test]
    fn a_complete_config_is_valid() {
        assert!(valid().validate().is_ok());
    }

    type Breaker = fn(&mut PublisherAuthnConfig);

    #[test]
    fn each_missing_or_malformed_field_is_refused() {
        let cases: Vec<(&str, Breaker)> = vec![
            ("empty project", |c| c.project_id.clear()),
            ("uppercase project", |c| {
                c.project_id = "Firebase-Test".to_owned();
            }),
            ("overlong project", |c| c.project_id = "a".repeat(64)),
            ("nil namespace", |c| c.subject_namespace = Uuid::nil()),
            ("nil tenant", |c| c.tenant_id = Uuid::nil()),
            ("no scopes", |c| c.token_scopes.clear()),
            ("blank scope", |c| c.token_scopes = vec![" ".to_owned()]),
        ];
        for (case, break_it) in cases {
            let mut config = valid();
            break_it(&mut config);
            assert!(config.validate().is_err(), "{case}");
        }
    }
}
