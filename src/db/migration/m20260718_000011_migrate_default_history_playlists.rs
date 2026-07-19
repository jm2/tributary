//! Migration: update untouched playback-history default smart playlists.
//!
//! Tributary historically seeded `Recently Played` from file modification
//! time and left `Top 25 Most Played` without a deterministic presentation
//! order.  There is no persisted "seeded by Tributary" marker, so this
//! migration deliberately recognizes only the complete byte-exact historical
//! signatures. This includes both the v0.5.0 JSON representation, whose rule
//! object contained `live_updating: true`, and its immediate successor after
//! that redundant JSON field was removed. Any user-visible or redundant-field
//! difference is treated as evidence that the playlist is user-owned and must
//! remain untouched.

use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::{ConnectionTrait, Statement, TransactionTrait};

const RECENTLY_PLAYED_NAME: &str = "Recently Played";
const TOP_25_NAME: &str = "Top 25 Most Played";

const LEGACY_RECENTLY_PLAYED_RULES: &str = r#"{"match_mode":"All","rules":[{"field":"DateModified","operator":{"IsInTheLast":{"amount":14,"unit":"Days"}},"value":{"Number":14}},{"field":"PlayCount","operator":"GreaterThan","value":{"Number":0}}],"limit":null,"sort_order":[{"field":"DateModified","direction":"Descending"}]}"#;
const LEGACY_TOP_25_RULES: &str = r#"{"match_mode":"All","rules":[{"field":"PlayCount","operator":"GreaterThan","value":{"Number":0}}],"limit":{"value":25,"unit":"Items","selected_by":"MostPlayed"},"sort_order":[]}"#;
// v0.5.0 serialized the database's always-live compatibility flag inside the
// rule object. Its position is part of the exact released signature.
const V0_5_0_RECENTLY_PLAYED_RULES: &str = r#"{"match_mode":"All","rules":[{"field":"DateModified","operator":{"IsInTheLast":{"amount":14,"unit":"Days"}},"value":{"Number":14}},{"field":"PlayCount","operator":"GreaterThan","value":{"Number":0}}],"limit":null,"live_updating":true,"sort_order":[{"field":"DateModified","direction":"Descending"}]}"#;
const V0_5_0_TOP_25_RULES: &str = r#"{"match_mode":"All","rules":[{"field":"PlayCount","operator":"GreaterThan","value":{"Number":0}}],"limit":{"value":25,"unit":"Items","selected_by":"MostPlayed"},"live_updating":true,"sort_order":[]}"#;

// These literals intentionally mirror `serde_json::to_string` for the fresh
// defaults. Keep them independent of the current Rust rule types: a migration
// must continue to recognize and emit its historical on-disk representation
// if those types evolve later.
const CURRENT_RECENTLY_PLAYED_RULES: &str = r#"{"match_mode":"All","rules":[{"field":"LastPlayed","operator":{"IsInTheLast":{"amount":14,"unit":"Days"}},"value":{"Number":14}}],"limit":null,"sort_order":[{"field":"LastPlayed","direction":"Descending"},{"field":"TrackId","direction":"Ascending"}]}"#;
const CURRENT_TOP_25_RULES: &str = r#"{"match_mode":"All","rules":[{"field":"PlayCount","operator":"GreaterThan","value":{"Number":0}}],"limit":{"value":25,"unit":"Items","selected_by":"MostPlayed"},"sort_order":[{"field":"PlayCount","direction":"Descending"},{"field":"LastPlayed","direction":"Descending"},{"field":"TrackId","direction":"Ascending"}]}"#;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        migrate_defaults(
            manager,
            &[LEGACY_RECENTLY_PLAYED_RULES, V0_5_0_RECENTLY_PLAYED_RULES],
            CURRENT_RECENTLY_PLAYED_RULES,
            &[LEGACY_TOP_25_RULES, V0_5_0_TOP_25_RULES],
            CURRENT_TOP_25_RULES,
        )
        .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        migrate_defaults(
            manager,
            &[CURRENT_RECENTLY_PLAYED_RULES],
            LEGACY_RECENTLY_PLAYED_RULES,
            &[CURRENT_TOP_25_RULES],
            LEGACY_TOP_25_RULES,
        )
        .await
    }
}

/// Update both matching defaults in one transaction.
///
/// SeaORM does not wrap a migration's body and its ledger update in the same
/// SQLite transaction. The guarded writes are therefore independently atomic
/// and idempotent: a failure rolls both back, while a process interruption
/// after commit but before the ledger write is safe to retry.
async fn migrate_defaults(
    manager: &SchemaManager<'_>,
    recently_played_sources: &[&str],
    recently_played_to: &str,
    top_25_sources: &[&str],
    top_25_to: &str,
) -> Result<(), DbErr> {
    if !manager.has_table("playlists").await? {
        return Err(DbErr::Migration(
            "playlists must exist for the default history-playlist migration".to_string(),
        ));
    }

    let transaction = manager.get_connection().begin().await?;
    let result = async {
        for from_rules in recently_played_sources {
            update_recently_played(&transaction, from_rules, recently_played_to).await?;
        }
        for from_rules in top_25_sources {
            update_top_25(&transaction, from_rules, top_25_to).await?;
        }
        Ok::<(), DbErr>(())
    }
    .await;

    match result {
        Ok(()) => transaction.commit().await,
        Err(error) => {
            transaction.rollback().await?;
            Err(error)
        }
    }
}

async fn update_recently_played<C: ConnectionTrait>(
    connection: &C,
    from_rules: &str,
    to_rules: &str,
) -> Result<(), DbErr> {
    connection
        .execute(Statement::from_sql_and_values(
            connection.get_database_backend(),
            "UPDATE playlists
             SET smart_rules_json = ?
             WHERE name = ?
               AND is_smart = 1
               AND smart_rules_json = ?
               AND limit_enabled = 0
               AND limit_value IS NULL
               AND limit_unit IS NULL
               AND limit_sort IS NULL
               AND match_mode = 'all'
               AND live_updating = 1",
            [
                to_rules.into(),
                RECENTLY_PLAYED_NAME.into(),
                from_rules.into(),
            ],
        ))
        .await?;
    Ok(())
}

async fn update_top_25<C: ConnectionTrait>(
    connection: &C,
    from_rules: &str,
    to_rules: &str,
) -> Result<(), DbErr> {
    connection
        .execute(Statement::from_sql_and_values(
            connection.get_database_backend(),
            "UPDATE playlists
             SET smart_rules_json = ?
             WHERE name = ?
               AND is_smart = 1
               AND smart_rules_json = ?
               AND limit_enabled = 1
               AND limit_value = 25
               AND limit_unit = '\"Items\"'
               AND limit_sort = '\"MostPlayed\"'
               AND match_mode = 'all'
               AND live_updating = 1",
            [to_rules.into(), TOP_25_NAME.into(), from_rules.into()],
        ))
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use sea_orm_migration::sea_orm::{
        ActiveModelTrait, ConnectionTrait, Database, DatabaseConnection, EntityTrait,
        IntoActiveModel, QueryOrder,
    };

    use super::*;
    use crate::db::entities::playlist;
    use crate::db::migration::Migrator;
    use crate::local::smart_rules::{
        DateUnit, LimitSort, LimitUnit, MatchMode, RuleField, RuleOperator, RuleValue, SmartLimit,
        SmartRule, SmartRules, SortCriterion, SortDirection, SortField,
    };

    async fn database_before_migration() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("open in-memory database");
        Migrator::up(&db, Some(10))
            .await
            .expect("apply migrations preceding default history playlists");
        db
    }

    fn recently_played_with_rules(
        id: impl Into<String>,
        smart_rules_json: &str,
    ) -> playlist::Model {
        playlist::Model {
            id: id.into(),
            name: RECENTLY_PLAYED_NAME.to_string(),
            is_smart: true,
            smart_rules_json: Some(smart_rules_json.to_string()),
            limit_enabled: false,
            limit_value: None,
            limit_unit: None,
            limit_sort: None,
            match_mode: "all".to_string(),
            live_updating: true,
            created_at: "2025-01-02T03:04:05.000Z".to_string(),
            updated_at: "2025-06-07T08:09:10.000Z".to_string(),
        }
    }

    fn recently_played(id: impl Into<String>) -> playlist::Model {
        recently_played_with_rules(id, LEGACY_RECENTLY_PLAYED_RULES)
    }

    fn v0_5_0_recently_played(id: impl Into<String>) -> playlist::Model {
        recently_played_with_rules(id, V0_5_0_RECENTLY_PLAYED_RULES)
    }

    fn top_25_with_rules(id: impl Into<String>, smart_rules_json: &str) -> playlist::Model {
        playlist::Model {
            id: id.into(),
            name: TOP_25_NAME.to_string(),
            is_smart: true,
            smart_rules_json: Some(smart_rules_json.to_string()),
            limit_enabled: true,
            limit_value: Some(25),
            limit_unit: Some("\"Items\"".to_string()),
            limit_sort: Some("\"MostPlayed\"".to_string()),
            match_mode: "all".to_string(),
            live_updating: true,
            created_at: "2025-11-12T13:14:15.000Z".to_string(),
            updated_at: "2026-01-02T03:04:05.000Z".to_string(),
        }
    }

    fn top_25(id: impl Into<String>) -> playlist::Model {
        top_25_with_rules(id, LEGACY_TOP_25_RULES)
    }

    fn v0_5_0_top_25(id: impl Into<String>) -> playlist::Model {
        top_25_with_rules(id, V0_5_0_TOP_25_RULES)
    }

    async fn insert_playlists(db: &DatabaseConnection, playlists: &[playlist::Model]) {
        for model in playlists {
            model
                .clone()
                .into_active_model()
                .insert(db)
                .await
                .expect("insert playlist fixture");
        }
    }

    async fn all_playlists(db: &DatabaseConnection) -> Vec<playlist::Model> {
        playlist::Entity::find()
            .order_by_asc(playlist::Column::Id)
            .all(db)
            .await
            .expect("load playlist fixtures")
    }

    async fn migration_is_applied(db: &DatabaseConnection) -> bool {
        let migration_name = Migration.name().to_string();
        Migrator::get_migration_models(db)
            .await
            .expect("query migration ledger")
            .iter()
            .any(|migration| migration.version == migration_name)
    }

    fn signature_near_misses() -> Vec<playlist::Model> {
        let mut models = Vec::new();
        let mut add_recent = |suffix: &str, change: fn(&mut playlist::Model)| {
            let mut model = recently_played(format!("recent-{suffix}"));
            change(&mut model);
            models.push(model);
        };
        add_recent("renamed", |model| model.name.push_str(" (renamed)"));
        add_recent("edited", |model| {
            model.smart_rules_json =
                Some(LEGACY_RECENTLY_PLAYED_RULES.replacen("\"amount\":14", "\"amount\":7", 1));
        });
        add_recent("reformatted", |model| {
            model.smart_rules_json = Some(
                serde_json::to_string_pretty(
                    &serde_json::from_str::<serde_json::Value>(LEGACY_RECENTLY_PLAYED_RULES)
                        .expect("parse historical Recently Played rules"),
                )
                .expect("reformat historical Recently Played rules"),
            );
        });
        add_recent("regular", |model| model.is_smart = false);
        add_recent("limit-enabled", |model| model.limit_enabled = true);
        add_recent("limit-value", |model| model.limit_value = Some(25));
        add_recent("limit-unit", |model| {
            model.limit_unit = Some("\"Items\"".to_string());
        });
        add_recent("limit-sort", |model| {
            model.limit_sort = Some("\"MostPlayed\"".to_string());
        });
        add_recent("match-mode", |model| model.match_mode = "any".to_string());
        add_recent("not-live", |model| model.live_updating = false);
        let mut v0_5_0_recent_false = v0_5_0_recently_played("v050-recent-false");
        v0_5_0_recent_false.smart_rules_json = Some(V0_5_0_RECENTLY_PLAYED_RULES.replacen(
            "\"live_updating\":true",
            "\"live_updating\":false",
            1,
        ));
        v0_5_0_recent_false.live_updating = false;
        models.push(v0_5_0_recent_false);
        let mut v0_5_0_recent_reformatted = v0_5_0_recently_played("v050-recent-reformatted");
        v0_5_0_recent_reformatted.smart_rules_json = Some(
            serde_json::to_string_pretty(
                &serde_json::from_str::<serde_json::Value>(V0_5_0_RECENTLY_PLAYED_RULES)
                    .expect("parse v0.5.0 Recently Played rules"),
            )
            .expect("reformat v0.5.0 Recently Played rules"),
        );
        models.push(v0_5_0_recent_reformatted);

        let mut add_top = |suffix: &str, change: fn(&mut playlist::Model)| {
            let mut model = top_25(format!("top-{suffix}"));
            change(&mut model);
            models.push(model);
        };
        add_top("renamed", |model| model.name.push_str(" (renamed)"));
        add_top("edited", |model| {
            model.smart_rules_json =
                Some(LEGACY_TOP_25_RULES.replacen("\"value\":25", "\"value\":20", 1));
        });
        add_top("reformatted", |model| {
            model.smart_rules_json = Some(
                serde_json::to_string_pretty(
                    &serde_json::from_str::<serde_json::Value>(LEGACY_TOP_25_RULES)
                        .expect("parse historical Top 25 rules"),
                )
                .expect("reformat historical Top 25 rules"),
            );
        });
        add_top("regular", |model| model.is_smart = false);
        add_top("limit-disabled", |model| model.limit_enabled = false);
        add_top("limit-value", |model| model.limit_value = Some(24));
        add_top("limit-unit", |model| {
            model.limit_unit = Some("\"Minutes\"".to_string());
        });
        add_top("limit-sort", |model| {
            model.limit_sort = Some("\"LeastPlayed\"".to_string());
        });
        add_top("match-mode", |model| model.match_mode = "any".to_string());
        add_top("not-live", |model| model.live_updating = false);
        let mut v0_5_0_top_false = v0_5_0_top_25("v050-top-false");
        v0_5_0_top_false.smart_rules_json = Some(V0_5_0_TOP_25_RULES.replacen(
            "\"live_updating\":true",
            "\"live_updating\":false",
            1,
        ));
        v0_5_0_top_false.live_updating = false;
        models.push(v0_5_0_top_false);
        let mut v0_5_0_top_reformatted = v0_5_0_top_25("v050-top-reformatted");
        v0_5_0_top_reformatted.smart_rules_json = Some(
            serde_json::to_string_pretty(
                &serde_json::from_str::<serde_json::Value>(V0_5_0_TOP_25_RULES)
                    .expect("parse v0.5.0 Top 25 rules"),
            )
            .expect("reformat v0.5.0 Top 25 rules"),
        );
        models.push(v0_5_0_top_reformatted);
        models
    }

    #[derive(serde::Serialize)]
    struct V0_5_0SmartRules {
        match_mode: MatchMode,
        rules: Vec<SmartRule>,
        limit: Option<SmartLimit>,
        live_updating: bool,
        sort_order: Vec<SortCriterion>,
    }

    #[test]
    fn legacy_rule_literals_match_both_historical_seed_serializers() {
        let recently_played_rules = SmartRules {
            match_mode: MatchMode::All,
            rules: vec![
                SmartRule {
                    field: RuleField::DateModified,
                    operator: RuleOperator::IsInTheLast {
                        amount: 14,
                        unit: DateUnit::Days,
                    },
                    value: RuleValue::Number(14),
                },
                SmartRule {
                    field: RuleField::PlayCount,
                    operator: RuleOperator::GreaterThan,
                    value: RuleValue::Number(0),
                },
            ],
            limit: None,
            sort_order: vec![SortCriterion {
                field: SortField::DateModified,
                direction: SortDirection::Descending,
            }],
        };
        let top_25_rules = SmartRules {
            match_mode: MatchMode::All,
            rules: vec![SmartRule {
                field: RuleField::PlayCount,
                operator: RuleOperator::GreaterThan,
                value: RuleValue::Number(0),
            }],
            limit: Some(SmartLimit {
                value: 25,
                unit: LimitUnit::Items,
                selected_by: LimitSort::MostPlayed,
            }),
            sort_order: vec![],
        };
        let v0_5_0_recently_played_rules = V0_5_0SmartRules {
            match_mode: recently_played_rules.match_mode.clone(),
            rules: recently_played_rules.rules.clone(),
            limit: recently_played_rules.limit.clone(),
            live_updating: true,
            sort_order: recently_played_rules.sort_order.clone(),
        };
        let v0_5_0_top_25_rules = V0_5_0SmartRules {
            match_mode: top_25_rules.match_mode.clone(),
            rules: top_25_rules.rules.clone(),
            limit: top_25_rules.limit.clone(),
            live_updating: true,
            sort_order: top_25_rules.sort_order.clone(),
        };

        assert_eq!(
            serde_json::to_string(&recently_played_rules).expect("serialize historical rules"),
            LEGACY_RECENTLY_PLAYED_RULES
        );
        assert_eq!(
            serde_json::to_string(&top_25_rules).expect("serialize historical rules"),
            LEGACY_TOP_25_RULES
        );
        assert_eq!(
            serde_json::to_string(&v0_5_0_recently_played_rules)
                .expect("serialize v0.5.0 Recently Played rules"),
            V0_5_0_RECENTLY_PLAYED_RULES
        );
        assert_eq!(
            serde_json::to_string(&v0_5_0_top_25_rules).expect("serialize v0.5.0 Top 25 rules"),
            V0_5_0_TOP_25_RULES
        );
    }

    #[tokio::test]
    async fn up_updates_only_complete_byte_exact_historical_signatures() {
        let db = database_before_migration().await;
        let exact = vec![
            recently_played("exact-recent"),
            top_25("exact-top"),
            v0_5_0_recently_played("exact-v050-recent"),
            v0_5_0_top_25("exact-v050-top"),
        ];
        let near_misses = signature_near_misses();
        let mut fixtures = exact.clone();
        fixtures.extend(near_misses.clone());
        insert_playlists(&db, &fixtures).await;

        Migrator::up(&db, Some(1))
            .await
            .expect("migrate untouched history defaults");

        let mut expected = near_misses;
        expected.extend(exact.into_iter().map(|mut model| {
            model.smart_rules_json = Some(
                if model.name == RECENTLY_PLAYED_NAME {
                    CURRENT_RECENTLY_PLAYED_RULES
                } else {
                    CURRENT_TOP_25_RULES
                }
                .to_string(),
            );
            model
        }));
        expected.sort_by(|left, right| left.id.cmp(&right.id));

        assert_eq!(all_playlists(&db).await, expected);
        assert!(migration_is_applied(&db).await);
    }

    #[tokio::test]
    async fn failure_rolls_back_both_updates_and_the_retry_is_safe() {
        let db = database_before_migration().await;
        let original = vec![
            recently_played("recent-no-field"),
            v0_5_0_recently_played("recent-v050"),
            top_25("top-no-field"),
            v0_5_0_top_25("top-v050"),
        ];
        insert_playlists(&db, &original).await;
        db.execute_unprepared(
            "CREATE TRIGGER fail_top_25_migration
             BEFORE UPDATE OF smart_rules_json ON playlists
             WHEN OLD.id = 'top-v050'
             BEGIN
                 SELECT RAISE(ABORT, 'forced Top 25 migration failure');
             END",
        )
        .await
        .expect("install migration failure trigger");

        Migrator::up(&db, Some(1))
            .await
            .expect_err("forced second update must fail the migration");
        assert_eq!(all_playlists(&db).await, original);
        assert!(!migration_is_applied(&db).await);

        db.execute_unprepared("DROP TRIGGER fail_top_25_migration")
            .await
            .expect("remove migration failure trigger");
        Migrator::up(&db, Some(1))
            .await
            .expect("retry atomic history-default migration");
        assert!(migration_is_applied(&db).await);

        let manager = SchemaManager::new(&db);
        Migration
            .up(&manager)
            .await
            .expect("repeat committed migration body");
        let migrated = all_playlists(&db).await;
        let expected = original
            .into_iter()
            .map(|mut model| {
                model.smart_rules_json = Some(
                    if model.name == RECENTLY_PLAYED_NAME {
                        CURRENT_RECENTLY_PLAYED_RULES
                    } else {
                        CURRENT_TOP_25_RULES
                    }
                    .to_string(),
                );
                model
            })
            .collect::<Vec<_>>();
        assert_eq!(migrated, expected);
    }

    #[tokio::test]
    async fn down_reverts_only_still_untouched_current_signatures() {
        let db = database_before_migration().await;
        let original = vec![v0_5_0_recently_played("recent"), v0_5_0_top_25("top")];
        insert_playlists(&db, &original).await;
        Migrator::up(&db, Some(1))
            .await
            .expect("migrate history defaults");

        let mut edited = playlist::Entity::find_by_id("recent")
            .one(&db)
            .await
            .expect("load migrated Recently Played")
            .expect("migrated Recently Played exists")
            .into_active_model();
        edited.name = sea_orm_migration::sea_orm::Set("My Recent Plays".to_string());
        edited
            .update(&db)
            .await
            .expect("rename migrated Recently Played");

        Migrator::down(&db, Some(1))
            .await
            .expect("roll back default history migration");

        let rows = all_playlists(&db).await;
        assert_eq!(rows[0].name, "My Recent Plays");
        assert_eq!(
            rows[0].smart_rules_json.as_deref(),
            Some(CURRENT_RECENTLY_PLAYED_RULES),
            "a user-modified current signature must not be rewritten"
        );
        let mut canonical_predecessor_top = original[1].clone();
        canonical_predecessor_top.smart_rules_json = Some(LEGACY_TOP_25_RULES.to_string());
        assert_eq!(rows[1], canonical_predecessor_top);
        assert!(!migration_is_applied(&db).await);
    }
}
