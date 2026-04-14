//! SeaORM entity for the `playlist_entries` table.

use sea_orm::entity::prelude::*;

/// The `playlist_entries` table model.
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "playlist_entries")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,

    pub playlist_id: String,
    pub position: i32,
    pub track_id: Option<String>,

    /// Fingerprint fields for track rediscovery after library rebuild.
    pub match_title: String,
    pub match_artist: String,
    pub match_album: String,
    pub match_duration_secs: Option<i32>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::playlist::Entity",
        from = "Column::PlaylistId",
        to = "super::playlist::Column::Id",
        on_delete = "Cascade"
    )]
    Playlist,
    #[sea_orm(
        belongs_to = "super::track::Entity",
        from = "Column::TrackId",
        to = "super::track::Column::Id",
        on_delete = "SetNull"
    )]
    Track,
}

impl Related<super::playlist::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Playlist.def()
    }
}

impl Related<super::track::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Track.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
