//! Schema of the gear's own tables. Applied names are recorded: never rename one.
use sea_orm_migration::MigrationTrait;

mod m20261120_000001_publisher_init;

#[must_use]
pub fn all() -> Vec<Box<dyn MigrationTrait>> {
    vec![Box::new(m20261120_000001_publisher_init::Initial)]
}
