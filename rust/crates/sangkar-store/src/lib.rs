pub mod db;
pub mod lock;
pub mod migrate;
pub mod migrations;
pub mod pool;
pub mod publish;
pub mod read;
pub mod schema;

/// The SQLite surface the crates above are allowed to name, re-exported rather than declared:
/// `rusqlite` belongs to this crate's manifest alone (`sangkar-boundary`), and the indexer's
/// persist pass has to write through the `Transaction` this crate hands it
/// (`publish::WriteScope::transaction`). Nothing here replaces a wrapper: a caller that wants the
/// prepared-statement cache uses [`db::Conn`].
pub use rusqlite::{OptionalExtension, ToSql, Transaction, params};
