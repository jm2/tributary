//! Database layer — SeaORM entities, migrations, and connection management.

pub mod connection;
pub mod entities;
pub mod migration;
mod upgrade;

use sea_orm::{DbErr, SqliteTransactionMode, TransactionOptions, TransactionTrait};

/// Begin a transaction that may write, taking SQLite's write lock up front.
///
/// A plain `begin()` is `BEGIN DEFERRED`: the transaction becomes a writer
/// only at its first write. If it has already read by then and another pooled
/// connection holds or has just committed the write lock, SQLite fails the
/// upgrade with `SQLITE_BUSY` at once, because the busy timeout only applies
/// while no transaction is open. `BEGIN IMMEDIATE` acquires the lock at the
/// start, where the busy timeout does apply, so a read-then-write mutation
/// waits for a competing writer instead of failing.
///
/// Every transaction that writes must use this. Read-only transactions keep
/// `begin()` so they never wait for, or block, a writer.
pub async fn begin_write<C>(db: &C) -> Result<C::Transaction, DbErr>
where
    C: TransactionTrait,
{
    db.begin_with_options(TransactionOptions {
        sqlite_transaction_mode: Some(SqliteTransactionMode::Immediate),
        ..TransactionOptions::default()
    })
    .await
}
