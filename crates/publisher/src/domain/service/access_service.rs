//! Who is calling and what they may do: config admins, approved members, and everyone
//! else, who may only ask for access. The authn plugin admits any verified Firebase
//! user, so this is the whole of the access control.
use crate::domain::access::Directory;
use crate::domain::error::{DomainError, NoAccess, Result};
use crate::domain::identity::{Claims, claims};
use crate::domain::models::{AccessStatus, Person, Role, UserRecord};
use crate::domain::repos::{NewRequest, UserRepository};
use std::sync::Arc;
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// Longest note on a request, in characters.
pub const MAX_NOTE: usize = 300;
/// Pending requests at most: signing in is open to anyone, the table is not.
pub const MAX_PENDING: usize = 500;

/// Where a signed-in caller stands.
#[derive(Debug, Clone)]
pub enum Standing {
    Admin(Person),
    Member(Person),
    Pending(UserRecord),
    Rejected(UserRecord),
    /// Signed in, never asked.
    Unknown(Claims),
}

pub struct AccessService {
    admins: Directory,
    users: Arc<dyn UserRepository>,
}

impl AccessService {
    #[must_use]
    pub fn new(admins: Directory, users: Arc<dyn UserRepository>) -> Self {
        Self { admins, users }
    }

    pub async fn standing(&self, ctx: &SecurityContext) -> Result<Standing> {
        if let Some(admin) = self.admins.admin(ctx) {
            let mut admin = admin.clone();
            admin.name = claims(ctx).name;
            return Ok(Standing::Admin(admin));
        }
        Ok(match self.users.get(ctx.subject_id()).await? {
            None => Standing::Unknown(claims(ctx)),
            Some(user) => match user.status {
                AccessStatus::Approved => Standing::Member(Person {
                    subject_id: user.subject_id,
                    uid: user.uid,
                    email: user.email,
                    name: user.name,
                    role: Role::Member,
                }),
                AccessStatus::Pending => Standing::Pending(user),
                AccessStatus::Rejected => Standing::Rejected(user),
            },
        })
    }

    /// An admin or an approved member; anyone else is refused.
    pub async fn person(&self, ctx: &SecurityContext) -> Result<Person> {
        match self.standing(ctx).await? {
            Standing::Admin(person) | Standing::Member(person) => Ok(person),
            Standing::Pending(_) => Err(DomainError::NoAccess(NoAccess::Pending)),
            Standing::Rejected(_) => Err(DomainError::NoAccess(NoAccess::Rejected)),
            Standing::Unknown(_) => Err(DomainError::NoAccess(NoAccess::NotRegistered)),
        }
    }

    /// An admin; a member gets `AdminOnly`, anyone else their `NoAccess`.
    pub async fn admin(&self, ctx: &SecurityContext) -> Result<Person> {
        let person = self.person(ctx).await?;
        if person.role == Role::Admin {
            Ok(person)
        } else {
            Err(DomainError::AdminOnly)
        }
    }

    /// Ask for access, or edit the note of a request still pending.
    pub async fn request(&self, ctx: &SecurityContext, note: &str) -> Result<UserRecord> {
        let subject_id = ctx.subject_id();
        if self.admins.is_admin(subject_id) {
            return Err(DomainError::Conflict(
                "admins need no access request".into(),
            ));
        }
        let note = note.trim();
        if note.is_empty() {
            return Err(DomainError::Invalid(
                "say who you are and why you need access".into(),
            ));
        }
        if note.chars().count() > MAX_NOTE {
            return Err(DomainError::Invalid(format!(
                "the note is longer than {MAX_NOTE} characters"
            )));
        }
        match self.users.get(subject_id).await?.map(|u| u.status) {
            Some(AccessStatus::Rejected) => return Err(DomainError::NoAccess(NoAccess::Rejected)),
            Some(AccessStatus::Approved) => {
                return Err(DomainError::Conflict("access is already approved".into()));
            }
            Some(AccessStatus::Pending) => {}
            None => {
                if self.users.list(Some(AccessStatus::Pending)).await?.len() >= MAX_PENDING {
                    return Err(DomainError::TooMany(
                        "too many requests are waiting; try again later".into(),
                    ));
                }
            }
        }
        let claims = claims(ctx);
        let record = self
            .users
            .request(NewRequest {
                subject_id,
                uid: claims.uid,
                email: claims.email,
                name: claims.name,
                provider: claims.provider,
                note: note.to_owned(),
            })
            .await?;
        tracing::info!(uid = %record.uid, provider = ?record.provider, "access requested");
        Ok(record)
    }

    /// Every request, newest first.
    pub async fn requests(&self) -> Result<Vec<UserRecord>> {
        Ok(self.users.list(None).await?)
    }

    pub async fn pending(&self) -> Result<usize> {
        Ok(self.users.list(Some(AccessStatus::Pending)).await?.len())
    }

    /// The configured admins, by uid.
    #[must_use]
    pub fn admins(&self) -> Vec<Person> {
        let mut admins: Vec<Person> = self.admins.admins().map(|(_, p)| p.clone()).collect();
        admins.sort_by(|a, b| a.uid.cmp(&b.uid));
        admins
    }

    /// Approve or reject a request. Rejecting a member revokes their access;
    /// approving a rejected request admits them. Admins are configured, not decided.
    pub async fn decide(
        &self,
        admin: &Person,
        subject_id: Uuid,
        status: AccessStatus,
    ) -> Result<UserRecord> {
        if admin.role != Role::Admin {
            return Err(DomainError::AdminOnly);
        }
        if status == AccessStatus::Pending {
            return Err(DomainError::Invalid("decide approve or reject".into()));
        }
        if self.admins.is_admin(subject_id) {
            return Err(DomainError::Conflict(
                "admins are configured in publisher.yaml".into(),
            ));
        }
        let record = self
            .users
            .decide(subject_id, status, admin.label())
            .await?
            .ok_or(DomainError::NotFound)?;
        tracing::info!(uid = %record.uid, status = status.as_str(), by = admin.label(), "access decided");
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::RunKind;
    use crate::domain::repos::MemoryUsers;
    use base64::Engine as _;

    fn ctx(subject: Uuid, uid: &str) -> SecurityContext {
        let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let payload = serde_json::json!({
            "sub": uid, "email": format!("{uid}@example.com"), "email_verified": true,
            "name": format!("Name {uid}"),
            "firebase": {"sign_in_provider": "github.com"}
        });
        let token = format!("h.{}.s", encode(payload.to_string().as_bytes()));
        SecurityContext::builder()
            .subject_id(subject)
            .subject_tenant_id(crate::domain::access::TENANT)
            .subject_type("user")
            .bearer_token(token)
            .build()
            .unwrap()
    }

    fn service() -> AccessService {
        let config: crate::config::PublisherConfig = serde_json::from_value(serde_json::json!({
            "work_dir": "/w", "target_dir": "/t", "queue_dsn": "postgres://x",
            "firebase_project_id": "p",
            "admins": [{"uid": "root", "email": "root@example.com"}],
        }))
        .unwrap();
        let admin = crate::domain::access::subject_id(&config.subject_namespace, "p", "root");
        assert_eq!(
            admin,
            Directory::from_config(&config)
                .admins()
                .next()
                .unwrap()
                .0
                .to_owned()
        );
        AccessService::new(
            Directory::from_config(&config),
            Arc::new(MemoryUsers::default()),
        )
    }

    fn admin_ctx(service: &AccessService) -> SecurityContext {
        let subject = *service.admins.admins().next().unwrap().0;
        ctx(subject, "root")
    }

    /// The core of the access control: a verified stranger gets nothing until an
    /// admin approves them, and then PR CI only.
    #[tokio::test]
    async fn a_stranger_asks_waits_and_once_approved_runs_ci_only() {
        let access = service();
        let stranger = ctx(Uuid::from_u128(42), "dev");
        assert!(matches!(
            access.standing(&stranger).await.unwrap(),
            Standing::Unknown(Claims { ref uid, .. }) if uid == "dev"
        ));
        assert!(matches!(
            access.person(&stranger).await,
            Err(DomainError::NoAccess(NoAccess::NotRegistered))
        ));

        let request = access
            .request(&stranger, "  Вася Иванов, CI  ")
            .await
            .unwrap();
        assert_eq!(request.note, "Вася Иванов, CI");
        assert_eq!(request.email.as_deref(), Some("dev@example.com"));
        assert_eq!(request.provider.as_deref(), Some("github.com"));
        assert!(matches!(
            access.person(&stranger).await,
            Err(DomainError::NoAccess(NoAccess::Pending))
        ));
        assert!(matches!(
            access.admin(&stranger).await,
            Err(DomainError::NoAccess(_))
        ));

        let admin = access.admin(&admin_ctx(&access)).await.unwrap();
        let decided = access
            .decide(&admin, stranger.subject_id(), AccessStatus::Approved)
            .await
            .unwrap();
        assert_eq!(decided.decided_by.as_deref(), Some("root@example.com"));

        let member = access.person(&stranger).await.unwrap();
        assert_eq!(member.role, Role::Member);
        assert_eq!(member.subject_id, stranger.subject_id());
        assert!(member.may_start(RunKind::Ci));
        assert!(!member.may_start(RunKind::Release));
        let own = Some(member.subject_id);
        let foreign = Some(Uuid::from_u128(9));
        assert!(member.may_control(RunKind::Ci, own));
        assert!(!member.may_control(RunKind::Ci, foreign));
        assert!(!member.may_control(RunKind::Ci, None));
        assert!(!member.may_control(RunKind::Release, own));
        assert!(member.may_see(RunKind::Ci, own));
        assert!(!member.may_see(RunKind::Ci, foreign));
        assert!(!member.may_see(RunKind::Ci, None));
        assert!(member.may_see(RunKind::Release, foreign));
        assert!(member.may_see(RunKind::Release, None));
        assert!(matches!(
            access.admin(&stranger).await,
            Err(DomainError::AdminOnly)
        ));
        assert!(matches!(
            access
                .decide(&member, Uuid::from_u128(9), AccessStatus::Approved)
                .await,
            Err(DomainError::AdminOnly)
        ));

        // Revoked: back to nothing, and no new request.
        access
            .decide(&admin, stranger.subject_id(), AccessStatus::Rejected)
            .await
            .unwrap();
        assert!(matches!(
            access.person(&stranger).await,
            Err(DomainError::NoAccess(NoAccess::Rejected))
        ));
        assert!(matches!(
            access.request(&stranger, "again").await,
            Err(DomainError::NoAccess(NoAccess::Rejected))
        ));
    }

    #[tokio::test]
    async fn admins_come_from_config_and_cannot_be_decided() {
        let access = service();
        let ctx = admin_ctx(&access);
        let admin = access.admin(&ctx).await.unwrap();
        assert_eq!(admin.subject_id, ctx.subject_id());
        for kind in RunKind::ALL {
            assert!(admin.may_start(kind));
            assert!(admin.may_control(kind, Some(Uuid::from_u128(9))));
            assert!(admin.may_control(kind, None));
            assert!(admin.may_see(kind, None));
        }
        assert_eq!(admin.name.as_deref(), Some("Name root"));
        assert!(matches!(
            access.request(&ctx, "me").await,
            Err(DomainError::Conflict(_))
        ));
        assert!(matches!(
            access
                .decide(&admin, ctx.subject_id(), AccessStatus::Rejected)
                .await,
            Err(DomainError::Conflict(_))
        ));
        assert!(matches!(
            access
                .decide(&admin, Uuid::from_u128(7), AccessStatus::Approved)
                .await,
            Err(DomainError::NotFound)
        ));
    }

    #[tokio::test]
    async fn the_note_is_required_bounded_and_editable_while_pending() {
        let access = service();
        let who = ctx(Uuid::from_u128(5), "u5");
        for bad in ["", "   ", &"я".repeat(MAX_NOTE + 1)] {
            assert!(
                matches!(
                    access.request(&who, bad).await,
                    Err(DomainError::Invalid(_))
                ),
                "{bad:?}"
            );
        }
        access.request(&who, &"я".repeat(MAX_NOTE)).await.unwrap();
        let edited = access.request(&who, "second").await.unwrap();
        assert_eq!(edited.note, "second");
        assert_eq!(access.requests().await.unwrap().len(), 1);
        assert_eq!(access.pending().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn new_requests_stop_at_the_pending_cap() {
        let access = service();
        for i in 0..MAX_PENDING {
            let n = u128::try_from(i).unwrap() + 100;
            access
                .request(&ctx(Uuid::from_u128(n), &format!("u{n}")), "hi")
                .await
                .unwrap();
        }
        assert!(matches!(
            access
                .request(&ctx(Uuid::from_u128(99), "late"), "hi")
                .await,
            Err(DomainError::TooMany(_))
        ));
        // Someone already waiting may still edit their note.
        access
            .request(&ctx(Uuid::from_u128(100), "u100"), "edited")
            .await
            .unwrap();
    }
}
