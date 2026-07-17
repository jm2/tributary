//! SeaORM entity for files that could not be parsed during scanning.
//!
//! Storing a failed-parse record prevents every subsequent scan from
//! re-attempting the same unparseable file on every startup.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "unparseable_files")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub file_path: String,
    pub date_modified: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
