//! SeaORM entity for persisted library-root availability state.

use sea_orm::entity::prelude::*;

/// The `library_roots` table records the filesystem identity observed for a
/// configured root. Reconciliation uses it to distinguish an intentionally
/// empty mounted library from an empty mountpoint whose volume is absent.
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "library_roots")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub path: String,
    pub device_id: Option<String>,
    pub identity_confirmed: bool,
    pub is_available: bool,
    pub last_scan_complete: bool,
    pub last_checked_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
