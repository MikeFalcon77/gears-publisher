//! Schema of the gear's own tables. Applied names are recorded: never rename one.
use sea_orm_migration::MigrationTrait;

mod m20261120_000001_publisher_init;
mod m20261123_000001_publisher_users;
mod m20261124_000001_publisher_run_owner;

#[must_use]
pub fn all() -> Vec<Box<dyn MigrationTrait>> {
    vec![
        Box::new(m20261120_000001_publisher_init::Initial),
        Box::new(m20261123_000001_publisher_users::Users),
        Box::new(m20261124_000001_publisher_run_owner::RunOwner),
    ]
}
