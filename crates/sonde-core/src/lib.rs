//! Sonde's database layer: drivers, value model and introspection.
//!
//! This crate never depends on a UI framework.

pub mod cassandra;
pub mod complete;
pub mod config;
pub mod ddl;
pub mod driver;
pub mod edit;
pub mod elastic;
pub mod error;
pub mod export;
pub mod filter;
pub mod history;
pub mod kv;
pub mod mongo;
pub mod mssql;
pub mod mysql;
pub mod postgres;
pub mod runtime;
pub mod schema;
pub mod snowflake;
pub mod split;
pub mod sqlite;
pub(crate) mod stream;
pub mod update;
pub mod value;

pub use driver::{
    Canceller, ConnectionConfig, DbKind, Dialect, Driver, Language, PAGE_ROWS, PostgresConfig,
    QueryHandle, QuerySummary, Secret, ServerConfig, Session, SqliteConfig, TlsMode, connect,
    dialect,
};
pub use edit::EditStatement;
pub use error::{Error, Result};
pub use schema::{Catalog, TableDetail, TableRef};
pub use value::{ColumnMeta, ColumnOrigin, ResultPage, Value};

/// Make `ring` the TLS provider for every driver. The dependency graph pulls
/// in more than one provider, and rustls refuses to pick between them.
pub(crate) fn tls_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // Fails only when a provider is already installed, which is fine.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
