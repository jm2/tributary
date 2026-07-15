//! SeaORM entity for completed library-root reauthorization receipts.

use sea_orm::entity::prelude::*;

/// A durable receipt proving that one explicit root-reauthorization request
/// completed with the exact path and marker identity recorded here.
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "root_reauthorization_receipts")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub request_id: String,
    pub old_path: String,
    pub new_path: String,
    pub marker_identity: String,
    pub completed_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
