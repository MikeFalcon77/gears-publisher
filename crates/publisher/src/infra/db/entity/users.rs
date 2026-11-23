//! `publisher_users`: access requests and admins' decisions on them.
use sea_orm::entity::prelude::*;
use toolkit_db_macros::Scopable;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "publisher_users")]
#[secure(unrestricted)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub subject_id: Uuid,
    pub uid: String,
    pub email: Option<String>,
    pub name: Option<String>,
    pub provider: Option<String>,
    pub note: String,
    pub status: String,
    pub requested_at: DateTimeUtc,
    pub decided_at: Option<DateTimeUtc>,
    pub decided_by: Option<String>,
}
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
