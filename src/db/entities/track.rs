//! SeaORM entity for the `tracks` table.

use sea_orm::entity::prelude::*;

/// The `tracks` table model.
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "tracks")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,

    #[sea_orm(unique)]
    pub file_path: String,

    pub title: String,
    pub artist_name: String,
    pub album_title: String,
    pub genre: Option<String>,
    pub year: Option<i32>,
    pub track_number: Option<i32>,
    pub disc_number: Option<i32>,
    pub duration_secs: Option<i64>,
    pub bitrate_kbps: Option<i32>,
    pub sample_rate_hz: Option<i32>,
    pub format: Option<String>,
    pub play_count: i32,
    pub date_added: String,
    pub date_modified: String,
    pub file_size_bytes: Option<i64>,
}

/// No relations for now (flat schema).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
