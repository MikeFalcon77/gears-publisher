//! Run metadata and the run log in PostgreSQL, with a live fan-out.
use crate::domain::models::{LiveEvent, LogLine, NewLine, QueuedCi, RunRecord};
use crate::domain::ports::Journal;
use crate::domain::repos::RunRepository;
use crate::infra::db::entity::{ci_queue, log, runs};
use async_trait::async_trait;
use chrono::Utc;
use sea_orm::{Set, entity::prelude::*, sea_query::Expr};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use toolkit_db::secure::{
    AccessScope, SecureDeleteExt, SecureEntityExt, SecureInsertManyExt, SecureUpdateExt,
    secure_insert,
};
use toolkit_db::{DBProvider, DbError};

/// Longest stored log line; longer ones are cut.
const MAX_LINE: usize = 8 * 1024;
/// Lines written per INSERT.
const BATCH: usize = 500;
/// Lines waiting for the writer; a full queue slows the commands' output down.
const QUEUE: usize = 16 * 1024;

impl From<log::Model> for LogLine {
    fn from(m: log::Model) -> Self {
        Self {
            seq: m.id,
            run_id: m.run_id,
            step: m.step,
            attempt: m.attempt,
            stream: m.stream,
            line: m.line,
            ts: m.ts,
        }
    }
}

impl From<runs::Model> for RunRecord {
    fn from(m: runs::Model) -> Self {
        Self {
            run_id: m.run_id,
            started_by: m.started_by,
            started_by_subject: m.started_by_subject,
            dry_run: m.dry_run,
            created_at: m.created_at,
            facts: m.facts,
        }
    }
}

impl TryFrom<ci_queue::Model> for QueuedCi {
    type Error = anyhow::Error;

    fn try_from(m: ci_queue::Model) -> anyhow::Result<Self> {
        Ok(Self {
            id: m.id,
            pr: u64::try_from(m.pr)?,
            jobs: m.jobs.map(serde_json::from_value).transpose()?,
            requested_by: m.requested_by,
            requested_by_subject: m.requested_by_subject,
            queued_at: m.queued_at,
        })
    }
}

/// Log lines go through one writer task that inserts them in batches. With a single
/// writer, `seq` (the `BIGSERIAL` id) becomes visible in increasing order, so a reader
/// that resumes after the last `seq` it saw never skips a line committed late.
pub struct PgRunRepository {
    db: Arc<DBProvider<DbError>>,
    live: broadcast::Sender<LiveEvent>,
    writer: mpsc::Sender<NewLine>,
}

fn scope() -> AccessScope {
    AccessScope::allow_all()
}

impl PgRunRepository {
    /// Starts the log writer; call inside the runtime.
    #[must_use]
    pub fn new(db: DBProvider<DbError>) -> Self {
        let (live, _) = broadcast::channel(4096);
        let (writer, queue) = mpsc::channel(QUEUE);
        let db = Arc::new(db);
        tokio::spawn(write_log(Arc::clone(&db), queue, live.clone()));
        Self { db, live, writer }
    }

    async fn save_facts(&self, run_id: Uuid, facts: serde_json::Value) -> anyhow::Result<()> {
        let conn = self.db.conn()?;
        runs::Entity::update_many()
            .secure()
            .scope_with(&scope())
            .filter(sea_orm::Condition::all().add(runs::Column::RunId.eq(run_id)))
            .col_expr(runs::Column::Facts, Expr::value(facts))
            .exec(&conn)
            .await?;
        Ok(())
    }
}

#[async_trait]
impl RunRepository for PgRunRepository {
    fn subscribe(&self) -> broadcast::Receiver<LiveEvent> {
        self.live.subscribe()
    }

    fn notify(&self, run_id: Uuid) {
        let _ = self.live.send(LiveEvent::Changed(run_id));
    }

    async fn create_run(
        &self,
        run_id: Uuid,
        started_by: &str,
        started_by_subject: Uuid,
        dry_run: bool,
        facts: serde_json::Value,
    ) -> anyhow::Result<()> {
        let conn = self.db.conn()?;
        secure_insert::<runs::Entity>(
            runs::ActiveModel {
                run_id: Set(run_id),
                started_by: Set(started_by.to_owned()),
                started_by_subject: Set(Some(started_by_subject)),
                dry_run: Set(dry_run),
                created_at: Set(Utc::now()),
                facts: Set(facts),
            },
            &scope(),
            &conn,
        )
        .await?;
        Ok(())
    }

    async fn run(&self, run_id: Uuid) -> anyhow::Result<Option<RunRecord>> {
        let conn = self.db.conn()?;
        Ok(runs::Entity::find_by_id(run_id)
            .secure()
            .scope_with(&scope())
            .one(&conn)
            .await?
            .map(Into::into))
    }

    async fn runs(&self, ids: Vec<Uuid>) -> anyhow::Result<Vec<RunRecord>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.db.conn()?;
        Ok(runs::Entity::find()
            .secure()
            .scope_with(&scope())
            .filter(sea_orm::Condition::all().add(runs::Column::RunId.is_in(ids)))
            .all(&conn)
            .await?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    async fn lines(&self, run_id: Uuid, after: i64, limit: u64) -> anyhow::Result<Vec<LogLine>> {
        let conn = self.db.conn()?;
        Ok(log::Entity::find()
            .secure()
            .scope_with(&scope())
            .filter(
                sea_orm::Condition::all()
                    .add(log::Column::RunId.eq(run_id))
                    .add(log::Column::Id.gt(after)),
            )
            .order_by(log::Column::Id, sea_orm::Order::Asc)
            .limit(limit)
            .all(&conn)
            .await?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    async fn prune_log(&self, days: i64) -> anyhow::Result<u64> {
        let conn = self.db.conn()?;
        let cutoff = Utc::now() - chrono::Duration::days(days);
        let result = log::Entity::delete_many()
            .secure()
            .scope_with(&scope())
            .filter(sea_orm::Condition::all().add(log::Column::Ts.lt(cutoff)))
            .exec(&conn)
            .await?;
        Ok(result.rows_affected)
    }

    async fn enqueue_ci(&self, entry: QueuedCi) -> anyhow::Result<()> {
        let conn = self.db.conn()?;
        secure_insert::<ci_queue::Entity>(
            ci_queue::ActiveModel {
                id: Set(entry.id),
                pr: Set(i64::try_from(entry.pr)?),
                jobs: Set(entry.jobs.map(|ids| serde_json::json!(ids))),
                requested_by: Set(entry.requested_by),
                requested_by_subject: Set(entry.requested_by_subject),
                queued_at: Set(entry.queued_at),
            },
            &scope(),
            &conn,
        )
        .await?;
        Ok(())
    }

    async fn ci_queue(&self) -> anyhow::Result<Vec<QueuedCi>> {
        let conn = self.db.conn()?;
        ci_queue::Entity::find()
            .secure()
            .scope_with(&scope())
            .order_by(ci_queue::Column::QueuedAt, sea_orm::Order::Asc)
            .all(&conn)
            .await?
            .into_iter()
            .map(TryInto::try_into)
            .collect()
    }

    async fn take_ci(&self, id: Uuid) -> anyhow::Result<Option<QueuedCi>> {
        let conn = self.db.conn()?;
        let Some(entry) = ci_queue::Entity::find_by_id(id)
            .secure()
            .scope_with(&scope())
            .one(&conn)
            .await?
        else {
            return Ok(None);
        };
        // Whoever deletes the row owns the entry: of two racing takers, one gets `None`.
        let deleted = ci_queue::Entity::delete_many()
            .secure()
            .scope_with(&scope())
            .filter(sea_orm::Condition::all().add(ci_queue::Column::Id.eq(id)))
            .exec(&conn)
            .await?;
        if deleted.rows_affected == 0 {
            return Ok(None);
        }
        entry.try_into().map(Some)
    }
}

#[async_trait]
impl Journal for PgRunRepository {
    async fn log(&self, line: NewLine) {
        if self.writer.send(line).await.is_err() {
            tracing::warn!("the log writer has stopped; a line is lost");
        }
    }

    async fn facts(&self, run_id: Uuid, facts: serde_json::Value) {
        if let Err(error) = self.save_facts(run_id, facts).await {
            tracing::warn!(%error, %run_id, "failed to store run facts");
        }
        self.notify(run_id);
    }
}

fn truncate(mut text: String) -> String {
    if text.len() > MAX_LINE {
        let mut cut = MAX_LINE;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push('…');
    }
    text
}

/// The single log writer: takes whatever is queued (up to [`BATCH`]), inserts it with
/// one statement and publishes the stored lines.
async fn write_log(
    db: Arc<DBProvider<DbError>>,
    mut queue: mpsc::Receiver<NewLine>,
    live: broadcast::Sender<LiveEvent>,
) {
    let mut batch = Vec::with_capacity(BATCH);
    while queue.recv_many(&mut batch, BATCH).await > 0 {
        let now = Utc::now();
        let models: Vec<log::ActiveModel> = batch
            .drain(..)
            .map(|line| log::ActiveModel {
                id: sea_orm::NotSet,
                run_id: Set(line.run_id),
                step: Set(line.step),
                attempt: Set(i32::try_from(line.attempt).unwrap_or(i32::MAX)),
                stream: Set(line.stream.to_owned()),
                line: Set(truncate(line.line)),
                ts: Set(now),
            })
            .collect();
        let n = models.len();
        let mut stored = None;
        for delay in [0, 1, 5] {
            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
            match insert_lines(&db, models.clone()).await {
                Ok(rows) => {
                    stored = Some(rows);
                    break;
                }
                Err(error) => tracing::warn!(%error, lines = n, "failed to store log lines"),
            }
        }
        let Some(mut rows) = stored else {
            tracing::error!(lines = n, "log lines dropped after retries");
            continue;
        };
        rows.sort_by_key(|r| r.id);
        for row in rows {
            let _ = live.send(LiveEvent::Log(row.into()));
        }
    }
}

async fn insert_lines(
    db: &DBProvider<DbError>,
    models: Vec<log::ActiveModel>,
) -> anyhow::Result<Vec<log::Model>> {
    let conn = db.conn()?;
    Ok(log::Entity::insert_many(models)
        .secure()
        .scope_unchecked(&scope())?
        .exec_with_returning(&conn)
        .await?)
}
