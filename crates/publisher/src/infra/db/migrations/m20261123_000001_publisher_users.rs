use sea_orm_migration::prelude::*;

pub struct Users;

impl MigrationName for Users {
    fn name(&self) -> &'static str {
        "m20261123_000001_publisher_users"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Users {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE TABLE publisher_users (
                    subject_id UUID PRIMARY KEY,
                    uid TEXT NOT NULL,
                    email TEXT,
                    name TEXT,
                    provider TEXT,
                    note TEXT NOT NULL,
                    status TEXT NOT NULL CHECK (status IN ('pending', 'approved', 'rejected')),
                    requested_at TIMESTAMPTZ NOT NULL,
                    decided_at TIMESTAMPTZ,
                    decided_by TEXT
                );
                CREATE INDEX publisher_users_status ON publisher_users (status, requested_at);",
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DROP TABLE publisher_users;")
            .await?;
        Ok(())
    }
}
