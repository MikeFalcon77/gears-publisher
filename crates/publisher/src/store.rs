//! The gear's own tables: who started a run, its facts and its log.
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sea_orm::{Set, entity::prelude::*, sea_query::Expr};
use serde::Serialize;
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

pub mod runs {
    use sea_orm::entity::prelude::*;
    use toolkit_db_macros::Scopable;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "publisher_runs")]
    #[secure(unrestricted)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub run_id: Uuid,
        pub started_by: String,
        pub dry_run: bool,
        pub created_at: DateTimeUtc,
        pub facts: Json,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

pub mod log {
    use sea_orm::entity::prelude::*;
    use toolkit_db_macros::Scopable;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "publisher_run_log")]
    #[secure(unrestricted)]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub run_id: Uuid,
        pub step: String,
        pub attempt: i32,
        pub stream: String,
        pub line: String,
        pub ts: DateTimeUtc,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

/// One stored log line. `seq` increases with every line of every run.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct LogLine {
    pub seq: i64,
    pub run_id: Uuid,
    pub step: String,
    pub attempt: i32,
    /// `stdout`, `stderr`, `cmd` (a started command) or `info` (the publisher's own notes).
    pub stream: String,
    pub line: String,
    pub ts: DateTime<Utc>,
}

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

/// A line before it is stored.
#[derive(Debug, Clone)]
pub struct NewLine {
    pub run_id: Uuid,
    pub step: String,
    pub attempt: u32,
    pub stream: &'static str,
    pub line: String,
}

/// A run's metadata row.
#[derive(Debug, Clone, Serialize)]
pub struct RunRecord {
    pub run_id: Uuid,
    pub started_by: String,
    pub dry_run: bool,
    pub created_at: DateTime<Utc>,
    pub facts: serde_json::Value,
}

impl From<runs::Model> for RunRecord {
    fn from(m: runs::Model) -> Self {
        Self {
            run_id: m.run_id,
            started_by: m.started_by,
            dry_run: m.dry_run,
            created_at: m.created_at,
            facts: m.facts,
        }
    }
}

/// Pushed to live streams.
#[derive(Debug, Clone)]
pub enum LiveEvent {
    Log(LogLine),
    /// A step saved new facts, or the run's state may have changed.
    Changed(Uuid),
}

/// What steps write while they run. A failure to record never fails a step.
#[async_trait]
pub trait Journal: Send + Sync + 'static {
    async fn log(&self, line: NewLine);
    async fn facts(&self, run_id: Uuid, facts: serde_json::Value);
}

/// PostgreSQL journal with a live fan-out.
///
/// Log lines go through one writer task that inserts them in batches. With a single
/// writer, `seq` (the `BIGSERIAL` id) becomes visible in increasing order, so a reader
/// that resumes after the last `seq` it saw never skips a line committed late.
pub struct PgStore {
    db: Arc<DBProvider<DbError>>,
    live: broadcast::Sender<LiveEvent>,
    writer: mpsc::Sender<NewLine>,
}

fn scope() -> AccessScope {
    AccessScope::allow_all()
}

impl PgStore {
    /// Starts the log writer; call inside the runtime.
    #[must_use]
    pub fn new(db: DBProvider<DbError>) -> Self {
        let (live, _) = broadcast::channel(4096);
        let (writer, queue) = mpsc::channel(QUEUE);
        let db = Arc::new(db);
        tokio::spawn(write_log(Arc::clone(&db), queue, live.clone()));
        Self { db, live, writer }
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<LiveEvent> {
        self.live.subscribe()
    }

    pub fn notify(&self, run_id: Uuid) {
        let _ = self.live.send(LiveEvent::Changed(run_id));
    }

    pub async fn create_run(
        &self,
        run_id: Uuid,
        started_by: &str,
        dry_run: bool,
    ) -> anyhow::Result<()> {
        let conn = self.db.conn()?;
        secure_insert::<runs::Entity>(
            runs::ActiveModel {
                run_id: Set(run_id),
                started_by: Set(started_by.to_owned()),
                dry_run: Set(dry_run),
                created_at: Set(Utc::now()),
                facts: Set(serde_json::json!({})),
            },
            &scope(),
            &conn,
        )
        .await?;
        Ok(())
    }

    pub async fn run(&self, run_id: Uuid) -> anyhow::Result<Option<RunRecord>> {
        let conn = self.db.conn()?;
        Ok(runs::Entity::find_by_id(run_id)
            .secure()
            .scope_with(&scope())
            .one(&conn)
            .await?
            .map(Into::into))
    }

    pub async fn runs(&self, ids: Vec<Uuid>) -> anyhow::Result<Vec<RunRecord>> {
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

    /// Lines of a run after `after`, oldest first.
    pub async fn lines(
        &self,
        run_id: Uuid,
        after: i64,
        limit: u64,
    ) -> anyhow::Result<Vec<LogLine>> {
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

    /// Deletes the log of runs older than `days`; the runs themselves stay listed.
    pub async fn prune_log(&self, days: i64) -> anyhow::Result<u64> {
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
impl Journal for PgStore {
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

pub mod migrations {
    use sea_orm_migration::prelude::*;

    #[must_use]
    pub fn all() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(Initial)]
    }

    pub struct Initial;

    impl MigrationName for Initial {
        fn name(&self) -> &'static str {
            "m20261120_000001_publisher_init"
        }
    }

    #[async_trait::async_trait]
    impl MigrationTrait for Initial {
        async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
            manager
                .get_connection()
                .execute_unprepared(
                    "CREATE TABLE publisher_runs (
                        run_id UUID PRIMARY KEY,
                        started_by TEXT NOT NULL,
                        dry_run BOOLEAN NOT NULL,
                        created_at TIMESTAMPTZ NOT NULL,
                        facts JSONB NOT NULL DEFAULT '{}'::jsonb
                    );
                    CREATE TABLE publisher_run_log (
                        id BIGSERIAL PRIMARY KEY,
                        run_id UUID NOT NULL,
                        step TEXT NOT NULL,
                        attempt INTEGER NOT NULL,
                        stream TEXT NOT NULL,
                        line TEXT NOT NULL,
                        ts TIMESTAMPTZ NOT NULL
                    );
                    CREATE INDEX publisher_run_log_run ON publisher_run_log (run_id, id);",
                )
                .await?;
            Ok(())
        }

        async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
            manager
                .get_connection()
                .execute_unprepared("DROP TABLE publisher_run_log; DROP TABLE publisher_runs;")
                .await?;
            Ok(())
        }
    }
}

/// In-memory journal for tests.
#[cfg(test)]
#[derive(Default)]
pub struct MemoryJournal {
    pub lines: parking_lot::Mutex<Vec<NewLine>>,
    pub facts: parking_lot::Mutex<Vec<serde_json::Value>>,
}

#[cfg(test)]
#[async_trait]
impl Journal for MemoryJournal {
    async fn log(&self, line: NewLine) {
        self.lines.lock().push(line);
    }
    async fn facts(&self, _: Uuid, facts: serde_json::Value) {
        self.facts.lock().push(facts);
    }
}
