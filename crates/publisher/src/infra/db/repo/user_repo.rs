//! Access requests in PostgreSQL.
use crate::domain::models::{AccessStatus, UserRecord};
use crate::domain::repos::{NewRequest, UserRepository};
use crate::infra::db::entity::users;
use async_trait::async_trait;
use chrono::Utc;
use sea_orm::{Set, entity::prelude::*, sea_query::Expr};
use std::sync::Arc;
use toolkit_db::secure::{AccessScope, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_db::{DBProvider, DbError};

pub struct PgUserRepository {
    db: Arc<DBProvider<DbError>>,
}

impl PgUserRepository {
    #[must_use]
    pub fn new(db: DBProvider<DbError>) -> Self {
        Self { db: Arc::new(db) }
    }
}

fn scope() -> AccessScope {
    AccessScope::allow_all()
}

impl TryFrom<users::Model> for UserRecord {
    type Error = anyhow::Error;

    fn try_from(m: users::Model) -> anyhow::Result<Self> {
        let status = AccessStatus::parse(&m.status)
            .ok_or_else(|| anyhow::anyhow!("unknown access status `{}`", m.status))?;
        Ok(Self {
            subject_id: m.subject_id,
            uid: m.uid,
            email: m.email,
            name: m.name,
            provider: m.provider,
            note: m.note,
            status,
            requested_at: m.requested_at,
            decided_at: m.decided_at,
            decided_by: m.decided_by,
        })
    }
}

#[async_trait]
impl UserRepository for PgUserRepository {
    async fn get(&self, subject_id: Uuid) -> anyhow::Result<Option<UserRecord>> {
        let conn = self.db.conn()?;
        users::Entity::find_by_id(subject_id)
            .secure()
            .scope_with(&scope())
            .one(&conn)
            .await?
            .map(TryInto::try_into)
            .transpose()
    }

    async fn request(&self, request: NewRequest) -> anyhow::Result<UserRecord> {
        let subject_id = request.subject_id;
        if self.get(subject_id).await?.is_none() {
            let conn = self.db.conn()?;
            let inserted = secure_insert::<users::Entity>(
                users::ActiveModel {
                    subject_id: Set(subject_id),
                    uid: Set(request.uid.clone()),
                    email: Set(request.email.clone()),
                    name: Set(request.name.clone()),
                    provider: Set(request.provider.clone()),
                    note: Set(request.note.clone()),
                    status: Set(AccessStatus::Pending.as_str().to_owned()),
                    requested_at: Set(Utc::now()),
                    decided_at: Set(None),
                    decided_by: Set(None),
                },
                &scope(),
                &conn,
            )
            .await;
            match inserted {
                Ok(_) => {
                    return self
                        .get(subject_id)
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("request {subject_id} vanished"));
                }
                // A concurrent request by the same person inserted first: update below.
                // Any other failure is the error.
                Err(error) => {
                    if self.get(subject_id).await?.is_none() {
                        return Err(error.into());
                    }
                }
            }
        }
        let conn = self.db.conn()?;
        users::Entity::update_many()
            .secure()
            .scope_with(&scope())
            .filter(
                sea_orm::Condition::all()
                    .add(users::Column::SubjectId.eq(subject_id))
                    .add(users::Column::Status.eq(AccessStatus::Pending.as_str())),
            )
            .col_expr(users::Column::Note, Expr::value(request.note))
            .col_expr(users::Column::Email, Expr::value(request.email))
            .col_expr(users::Column::Name, Expr::value(request.name))
            .col_expr(users::Column::Provider, Expr::value(request.provider))
            .exec(&conn)
            .await?;
        self.get(subject_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("request {subject_id} vanished"))
    }

    async fn list(&self, status: Option<AccessStatus>) -> anyhow::Result<Vec<UserRecord>> {
        let conn = self.db.conn()?;
        let mut condition = sea_orm::Condition::all();
        if let Some(status) = status {
            condition = condition.add(users::Column::Status.eq(status.as_str()));
        }
        users::Entity::find()
            .secure()
            .scope_with(&scope())
            .filter(condition)
            .order_by(users::Column::RequestedAt, sea_orm::Order::Desc)
            .all(&conn)
            .await?
            .into_iter()
            .map(TryInto::try_into)
            .collect()
    }

    async fn decide(
        &self,
        subject_id: Uuid,
        status: AccessStatus,
        by: &str,
    ) -> anyhow::Result<Option<UserRecord>> {
        let conn = self.db.conn()?;
        users::Entity::update_many()
            .secure()
            .scope_with(&scope())
            .filter(sea_orm::Condition::all().add(users::Column::SubjectId.eq(subject_id)))
            .col_expr(users::Column::Status, Expr::value(status.as_str()))
            .col_expr(users::Column::DecidedAt, Expr::value(Utc::now()))
            .col_expr(users::Column::DecidedBy, Expr::value(by.to_owned()))
            .exec(&conn)
            .await?;
        self.get(subject_id).await
    }
}
