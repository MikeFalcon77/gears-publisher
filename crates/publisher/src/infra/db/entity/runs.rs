//! `publisher_runs`: who started a run and what its steps found.
use sea_orm::entity::prelude::*;
use toolkit_db_macros::Scopable;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "publisher_runs")]
#[secure(unrestricted)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub run_id: Uuid,
    pub started_by: String,
    /// `None` for runs recorded before owners were, unless a member's label matched.
    pub started_by_subject: Option<Uuid>,
    pub dry_run: bool,
    pub created_at: DateTimeUtc,
    pub facts: Json,
}
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
