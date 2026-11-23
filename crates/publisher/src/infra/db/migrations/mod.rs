//! Schema of the gear's own tables. Applied names are recorded: never rename one.
use sea_orm_migration::MigrationTrait;

mod m20261120_000001_publisher_init;
mod m20261123_000001_publisher_users;

#[must_use]
pub fn all() -> Vec<Box<dyn MigrationTrait>> {
    vec![
        Box::new(m20261120_000001_publisher_init::Initial),
        Box::new(m20261123_000001_publisher_users::Users),
    ]
}
