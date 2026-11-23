//! What the ID token says about the caller, beyond the subject id.
//!
//! `SecurityContext` carries no name or email, so they are read from the bearer token
//! it holds. The signature is not checked again: every authenticated route goes
//! through `publisher-authn-plugin`, which verified this very token before the context
//! existed. These claims only *name* people; who may do what is keyed by
//! `SecurityContext::subject_id`, never by a claim.
use base64::Engine as _;
use secrecy::ExposeSecret;
use serde::Deserialize;
use toolkit_security::SecurityContext;

/// Longest name or email kept; Firebase's are far shorter.
const MAX_CLAIM: usize = 256;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Claims {
    pub uid: String,
    /// Only a verified address: anyone can put any address on a token they mint
    /// through an unverified provider, and admins approve people by it.
    pub email: Option<String>,
    pub name: Option<String>,
    /// `google.com`, `github.com`, ...
    pub provider: Option<String>,
}

#[derive(Deserialize)]
struct Raw {
    #[serde(default)]
    sub: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    email_verified: bool,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    firebase: Option<Firebase>,
}

#[derive(Deserialize)]
struct Firebase {
    #[serde(default)]
    sign_in_provider: Option<String>,
}

/// The caller's claims; empty when the context has no readable token.
#[must_use]
pub fn claims(ctx: &SecurityContext) -> Claims {
    ctx.bearer_token()
        .and_then(|token| parse(token.expose_secret()))
        .unwrap_or_default()
}

fn parse(token: &str) -> Option<Claims> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let raw: Raw = serde_json::from_slice(&bytes).ok()?;
    Some(Claims {
        uid: raw.sub,
        email: clean(raw.email.filter(|_| raw.email_verified)),
        name: clean(raw.name),
        provider: clean(raw.firebase.and_then(|f| f.sign_in_provider)),
    })
}

fn clean(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().chars().take(MAX_CLAIM).collect::<String>())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(payload: &serde_json::Value) -> String {
        let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        format!(
            "{}.{}.sig",
            encode(br#"{"alg":"RS256"}"#),
            encode(payload.to_string().as_bytes())
        )
    }

    #[test]
    fn names_come_from_the_token() {
        let parsed = parse(&token(&serde_json::json!({
            "sub": "uid-1", "email": " a@example.com ", "email_verified": true, "name": "Вася Иванов",
            "firebase": {"sign_in_provider": "github.com"}
        })))
        .unwrap();
        assert_eq!(
            parsed,
            Claims {
                uid: "uid-1".into(),
                email: Some("a@example.com".into()),
                name: Some("Вася Иванов".into()),
                provider: Some("github.com".into()),
            }
        );
    }

    /// An address nobody verified must not name anyone to the admins.
    #[test]
    fn an_unverified_email_is_dropped() {
        let parsed = parse(&token(&serde_json::json!({
            "sub": "u", "email": "boss@example.com", "email_verified": false, "name": "Not The Boss"
        })))
        .unwrap();
        assert_eq!(parsed.email, None);
        assert_eq!(parsed.name.as_deref(), Some("Not The Boss"));
    }

    #[test]
    fn missing_or_blank_claims_are_none_and_garbage_is_empty() {
        let parsed = parse(&token(&serde_json::json!({"sub": "u", "email": " "}))).unwrap();
        assert_eq!(parsed.email, None);
        assert_eq!(parsed.name, None);
        assert_eq!(parse("not-a-token"), None);
        assert_eq!(claims(&SecurityContext::anonymous()), Claims::default());
    }
}
