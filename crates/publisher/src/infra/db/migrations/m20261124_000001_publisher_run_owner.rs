use sea_orm_migration::prelude::*;

/// Who owns a run, by subject id (members see only their own PR CI runs), and the
/// PR CI queue.
pub struct RunOwner;

impl MigrationName for RunOwner {
    fn name(&self) -> &'static str {
        "m20261124_000001_publisher_run_owner"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for RunOwner {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE publisher_runs ADD COLUMN started_by_subject UUID;
                CREATE INDEX publisher_runs_owner ON publisher_runs (started_by_subject);
                -- Members' earlier runs, by the label they were recorded under
                -- (Person::label). Admins' stay unowned: admins see every run.
                UPDATE publisher_runs r SET started_by_subject = u.subject_id
                    FROM publisher_users u
                    WHERE r.started_by_subject IS NULL
                      AND r.started_by = COALESCE(u.email, u.name, u.uid);
                CREATE TABLE publisher_ci_queue (
                    id UUID PRIMARY KEY,
                    pr BIGINT NOT NULL,
                    jobs JSONB,
                    requested_by TEXT NOT NULL,
                    requested_by_subject UUID NOT NULL,
                    queued_at TIMESTAMPTZ NOT NULL
                );
                CREATE INDEX publisher_ci_queue_order ON publisher_ci_queue (queued_at);",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "DROP TABLE publisher_ci_queue;
                ALTER TABLE publisher_runs DROP COLUMN started_by_subject;",
            )
            .await?;
        Ok(())
    }
}
