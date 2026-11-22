use sea_orm_migration::prelude::*;

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
