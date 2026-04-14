//! SeaORM entity for the `playlists` table.

use sea_orm::entity::prelude::*;

/// The `playlists` table model.
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "playlists")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,

    pub name: String,
    pub is_smart: bool,
    pub smart_rules_json: Option<String>,
    pub limit_enabled: bool,
    pub limit_value: Option<i32>,
    pub limit_unit: Option<String>,
    pub limit_sort: Option<String>,
    pub match_mode: String,
    pub live_updating: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::playlist_entry::Entity")]
    PlaylistEntries,
}

impl Related<super::playlist_entry::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::PlaylistEntries.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
