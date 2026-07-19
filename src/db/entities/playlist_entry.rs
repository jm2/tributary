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
    /// Stable owner of `track_id`. Together these two columns are the
    /// durable media identity; neither is a locator or session credential.
    pub source_id: String,
    /// Exact source-native track identity. This is nullable only for an
    /// unmatched legacy local import that has fingerprint evidence instead.
    pub track_id: Option<String>,
    /// Current local-table binding for a local entry. This is deliberately
    /// separate from durable identity so deleting a local track can make the
    /// occurrence unavailable without discarding its source-scoped key.
    pub local_track_id: Option<String>,

    /// Fingerprint fields for track rediscovery after library rebuild.
    pub match_title: String,
    pub match_artist: String,
    pub match_album: String,
    pub match_duration_secs: Option<i32>,
    /// Exact source path retained from an imported playlist, when provided.
    pub match_file_path: Option<String>,
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
        from = "Column::LocalTrackId",
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
