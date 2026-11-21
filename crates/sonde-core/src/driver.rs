//! The traits a database implements, and the types that cross them.
//!
//! Every future returned here can be awaited on any executor: the drivers do
//! their work on the crate's own runtime (see [`crate::runtime`]) and report
//! back through `futures` channels.

use std::{collections::BTreeMap, fmt, path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use futures::channel::{mpsc, oneshot};
use serde::{Deserialize, Serialize};

use crate::{
    edit::EditStatement,
    error::Result,
    schema::{Catalog, TableDetail, TableRef},
    value::{ColumnMeta, ResultPage, Value},
};

/// Rows per [`ResultPage`].
pub const PAGE_ROWS: usize = 1_000;
/// Pages a query may have waiting before it stops reading from the database.
pub(crate) const CHANNEL_PAGES: usize = 2;

/// The databases Sonde speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DbKind {
    /// PostgreSQL.
    Postgres,
    /// MySQL.
    Mysql,
    /// MariaDB, through the MySQL protocol.
    Mariadb,
    /// Microsoft SQL Server, and Azure SQL.
    SqlServer,
    /// SQLite.
    Sqlite,
    /// Snowflake, through its SQL REST API.
    Snowflake,
    /// MongoDB.
    Mongodb,
    /// Redis, and the servers that speak its protocol: Valkey, KeyDB,
    /// Dragonfly.
    Redis,
    /// Apache Cassandra, and ScyllaDB.
    Cassandra,
    /// Elasticsearch, and OpenSearch.
    Elasticsearch,
}

/// What the editor of a connection takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    /// SQL, in the database's own dialect.
    Sql,
    /// Cassandra's CQL: SQL-shaped, one statement per `;`.
    Cql,
    /// Redis commands, one per line, as `redis-cli` takes them.
    Redis,
    /// `mongosh`-style calls such as `db.orders.find({status: "open"})`.
    Mongo,
    /// SQL, or REST requests written the way Kibana's console takes them
    /// (`GET /index/_search` and a JSON body).
    Elastic,
}

impl DbKind {
    /// Every kind, in the order a chooser lists them.
    pub const ALL: [Self; 10] = [
        Self::Postgres,
        Self::Mysql,
        Self::Mariadb,
        Self::SqlServer,
        Self::Sqlite,
        Self::Snowflake,
        Self::Mongodb,
        Self::Redis,
        Self::Cassandra,
        Self::Elasticsearch,
    ];

    /// The product's name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Postgres => "PostgreSQL",
            Self::Mysql => "MySQL",
            Self::Mariadb => "MariaDB",
            Self::SqlServer => "SQL Server",
            Self::Sqlite => "SQLite",
            Self::Snowflake => "Snowflake",
            Self::Mongodb => "MongoDB",
            Self::Redis => "Redis",
            Self::Cassandra => "Cassandra",
            Self::Elasticsearch => "Elasticsearch",
        }
    }

    /// Other products the same driver serves, for a chooser to mention.
    pub fn also(self) -> Option<&'static str> {
        match self {
            Self::Redis => Some("Valkey, KeyDB, Dragonfly"),
            Self::Cassandra => Some("ScyllaDB"),
            Self::Elasticsearch => Some("OpenSearch"),
            Self::SqlServer => Some("Azure SQL"),
            _ => None,
        }
    }

    /// A short label for badges.
    pub fn badge(self) -> &'static str {
        match self {
            Self::Postgres => "PG",
            Self::Mysql => "MySQL",
            Self::Mariadb => "Maria",
            Self::SqlServer => "MSSQL",
            Self::Sqlite => "SQLite",
            Self::Snowflake => "SF",
            Self::Mongodb => "Mongo",
            Self::Redis => "Redis",
            Self::Cassandra => "CQL",
            Self::Elasticsearch => "ES",
        }
    }

    /// The language the editor takes.
    pub fn language(self) -> Language {
        match self {
            Self::Redis => Language::Redis,
            Self::Mongodb => Language::Mongo,
            Self::Cassandra => Language::Cql,
            Self::Elasticsearch => Language::Elastic,
            _ => Language::Sql,
        }
    }

    /// Whether this is a server reached over the network, with a host,
    /// port, user and password, rather than a file.
    pub fn is_server(self) -> bool {
        !matches!(self, Self::Sqlite)
    }

    /// Whether a login is optional: these servers commonly run without one.
    pub fn user_optional(self) -> bool {
        matches!(
            self,
            Self::Redis | Self::Mongodb | Self::Cassandra | Self::Elasticsearch | Self::Snowflake
        )
    }

    /// What the connection form calls the `database` field, or `None` when
    /// the kind has no use for one.
    pub fn database_label(self) -> Option<&'static str> {
        match self {
            Self::Redis => Some("Database number"),
            Self::Cassandra => Some("Keyspace"),
            Self::Elasticsearch => None,
            _ => Some("Database"),
        }
    }

    /// The port the server listens on unless told otherwise.
    pub fn default_port(self) -> Option<u16> {
        match self {
            Self::Postgres => Some(5432),
            Self::Mysql | Self::Mariadb => Some(3306),
            Self::SqlServer => Some(1433),
            Self::Sqlite => None,
            Self::Snowflake => Some(443),
            Self::Mongodb => Some(27017),
            Self::Redis => Some(6379),
            Self::Cassandra => Some(9042),
            Self::Elasticsearch => Some(9200),
        }
    }

    /// Whether the server holds several databases, so the connection can
    /// show the one it names or all of them (see
    /// [`ServerConfig::all_databases`]).
    pub fn lists_databases(self) -> bool {
        self.session_per_database() || crate::ddl::schemas_are_databases(self)
    }

    /// Whether a session reaches only the database it opened, so showing
    /// every database takes a session for each. The other kinds that list
    /// databases (MySQL, MongoDB, Cassandra) see them all from one session,
    /// as schemas.
    pub fn session_per_database(self) -> bool {
        matches!(
            self,
            Self::Postgres | Self::SqlServer | Self::Snowflake | Self::Redis
        )
    }

    /// Extra settings the kind takes, as `(key, label, placeholder)`. They
    /// live in [`ServerConfig::options`].
    pub fn option_fields(self) -> &'static [(&'static str, &'static str, &'static str)] {
        match self {
            Self::Snowflake => &[
                ("warehouse", "Warehouse", "COMPUTE_WH"),
                ("role", "Role", "optional"),
                ("schema", "Schema", "PUBLIC"),
            ],
            Self::Mongodb => &[(
                "options",
                "Connection options",
                "authSource=admin&replicaSet=rs0",
            )],
            _ => &[],
        }
    }
}

/// How a server connection uses TLS. The names follow libpq's `sslmode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TlsMode {
    /// Never use TLS.
    Disable,
    /// Use TLS if the server offers it, without checking its certificate.
    #[default]
    Prefer,
    /// Insist on TLS, without checking the server's certificate.
    Require,
    /// Insist on TLS and check the certificate against the system's roots and
    /// the host name.
    VerifyFull,
}

/// Where a database server is and who to be.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Host name or address.
    pub host: String,
    /// TCP port.
    pub port: u16,
    /// Database to open.
    pub database: String,
    /// Role to log in as.
    pub user: String,
    /// TLS policy.
    #[serde(default)]
    pub tls: TlsMode,
    /// Settings particular to one kind; see [`DbKind::option_fields`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub options: BTreeMap<String, String>,
}

impl ServerConfig {
    /// The option that asks the sidebar to list every database.
    pub const ALL_DATABASES: &str = "all_databases";

    /// Whether the sidebar lists every database on the server, each with
    /// its own session, rather than only the one this connection opens.
    pub fn all_databases(&self) -> bool {
        self.option(Self::ALL_DATABASES) == Some("true")
    }

    /// A setting from [`options`](Self::options), when set and not blank.
    pub fn option(&self, key: &str) -> Option<&str> {
        self.options
            .get(key)
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
    }
}

/// Where a Postgres server is and who to be.
pub type PostgresConfig = ServerConfig;

/// Which SQLite database to open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqliteConfig {
    /// Path to the file, or `:memory:` for a private in-memory database.
    pub path: PathBuf,
    /// Open without the ability to write.
    #[serde(default)]
    pub read_only: bool,
}

/// What to connect to. Never holds a password; see [`Secret`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ConnectionConfig {
    /// A Postgres server.
    Postgres(ServerConfig),
    /// A MySQL server.
    Mysql(ServerConfig),
    /// A MariaDB server.
    Mariadb(ServerConfig),
    /// A SQL Server.
    #[serde(rename = "sqlserver")]
    SqlServer(ServerConfig),
    /// A SQLite file.
    Sqlite(SqliteConfig),
    /// A Snowflake account. `host` is the account identifier or its host
    /// name.
    Snowflake(ServerConfig),
    /// A MongoDB server or replica set.
    Mongodb(ServerConfig),
    /// A Redis-protocol server.
    Redis(ServerConfig),
    /// A Cassandra or ScyllaDB cluster.
    Cassandra(ServerConfig),
    /// An Elasticsearch or OpenSearch cluster.
    Elasticsearch(ServerConfig),
}

impl ConnectionConfig {
    /// The kind of database this points at.
    pub fn kind(&self) -> DbKind {
        match self {
            Self::Postgres(_) => DbKind::Postgres,
            Self::Mysql(_) => DbKind::Mysql,
            Self::Mariadb(_) => DbKind::Mariadb,
            Self::SqlServer(_) => DbKind::SqlServer,
            Self::Sqlite(_) => DbKind::Sqlite,
            Self::Snowflake(_) => DbKind::Snowflake,
            Self::Mongodb(_) => DbKind::Mongodb,
            Self::Redis(_) => DbKind::Redis,
            Self::Cassandra(_) => DbKind::Cassandra,
            Self::Elasticsearch(_) => DbKind::Elasticsearch,
        }
    }

    /// The server details, for the kinds that are servers.
    pub fn server(&self) -> Option<&ServerConfig> {
        match self {
            Self::Postgres(server)
            | Self::Mysql(server)
            | Self::Mariadb(server)
            | Self::SqlServer(server)
            | Self::Snowflake(server)
            | Self::Mongodb(server)
            | Self::Redis(server)
            | Self::Cassandra(server)
            | Self::Elasticsearch(server) => Some(server),
            Self::Sqlite(_) => None,
        }
    }

    /// The same connection, opening database `name` as listed by
    /// [`Session::databases`]. Redis databases are listed as `db3`.
    pub fn with_database(&self, name: &str) -> Self {
        let mut config = self.clone();
        let kind = config.kind();
        if let Some(server) = config.server_mut() {
            server.database = if kind == DbKind::Redis {
                name.trim_start_matches("db").to_owned()
            } else {
                name.to_owned()
            };
        }
        config
    }

    fn server_mut(&mut self) -> Option<&mut ServerConfig> {
        match self {
            Self::Postgres(server)
            | Self::Mysql(server)
            | Self::Mariadb(server)
            | Self::SqlServer(server)
            | Self::Snowflake(server)
            | Self::Mongodb(server)
            | Self::Redis(server)
            | Self::Cassandra(server)
            | Self::Elasticsearch(server) => Some(server),
            Self::Sqlite(_) => None,
        }
    }

    /// A server connection of `kind`. `None` for a kind that is not a
    /// server.
    pub fn for_server(kind: DbKind, server: ServerConfig) -> Option<Self> {
        match kind {
            DbKind::Postgres => Some(Self::Postgres(server)),
            DbKind::Mysql => Some(Self::Mysql(server)),
            DbKind::Mariadb => Some(Self::Mariadb(server)),
            DbKind::SqlServer => Some(Self::SqlServer(server)),
            DbKind::Sqlite => None,
            DbKind::Snowflake => Some(Self::Snowflake(server)),
            DbKind::Mongodb => Some(Self::Mongodb(server)),
            DbKind::Redis => Some(Self::Redis(server)),
            DbKind::Cassandra => Some(Self::Cassandra(server)),
            DbKind::Elasticsearch => Some(Self::Elasticsearch(server)),
        }
    }
}

/// A password. Its `Debug` output never shows the text.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wrap a password.
    pub fn new(secret: impl Into<String>) -> Self {
        Self(secret.into())
    }

    /// The password itself, for handing to a driver or a keyring.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

/// Opens sessions for one kind of database.
#[async_trait]
pub trait Driver: Send + Sync {
    /// The database this driver speaks.
    fn kind(&self) -> DbKind;

    /// Open a session.
    ///
    /// # Errors
    ///
    /// [`Error::Connect`](crate::Error::Connect) when the database cannot be
    /// reached or refuses the login, and
    /// [`Error::Config`](crate::Error::Config) when `cfg` is for another
    /// driver.
    async fn connect(
        &self,
        cfg: &ConnectionConfig,
        secret: Option<Secret>,
    ) -> Result<Box<dyn Session>>;
}

/// The driver for `kind`.
pub fn driver(kind: DbKind) -> &'static dyn Driver {
    match kind {
        DbKind::Postgres => &crate::postgres::PostgresDriver,
        DbKind::Mysql => &crate::mysql::MysqlDriver,
        DbKind::Mariadb => &crate::mysql::MariadbDriver,
        DbKind::SqlServer => &crate::mssql::MssqlDriver,
        DbKind::Sqlite => &crate::sqlite::SqliteDriver,
        DbKind::Snowflake => &crate::snowflake::SnowflakeDriver,
        DbKind::Mongodb => &crate::mongo::MongoDriver,
        DbKind::Redis => &crate::kv::RedisDriver,
        DbKind::Cassandra => &crate::cassandra::CassandraDriver,
        DbKind::Elasticsearch => &crate::elastic::ElasticDriver,
    }
}

/// The SQL spelling of `kind`, for code that has no session at hand.
pub fn dialect(kind: DbKind) -> &'static dyn Dialect {
    match kind {
        DbKind::Postgres => &crate::postgres::PostgresDialect,
        DbKind::Mysql => &crate::mysql::MysqlDialect,
        DbKind::Mariadb => &crate::mysql::MariadbDialect,
        DbKind::SqlServer => &crate::mssql::MssqlDialect,
        DbKind::Sqlite => &crate::sqlite::SqliteDialect,
        DbKind::Snowflake => &crate::snowflake::SnowflakeDialect,
        DbKind::Mongodb => &crate::mongo::MongoDialect,
        DbKind::Redis => &crate::kv::RedisDialect,
        DbKind::Cassandra => &crate::cassandra::CassandraDialect,
        DbKind::Elasticsearch => &crate::elastic::ElasticDialect,
    }
}

/// Open a session with the driver `cfg` calls for.
///
/// # Errors
///
/// See [`Driver::connect`].
pub async fn connect(cfg: &ConnectionConfig, secret: Option<Secret>) -> Result<Box<dyn Session>> {
    driver(cfg.kind()).connect(cfg, secret).await
}

/// One open connection.
///
/// A session runs one statement at a time. A statement whose rows have not
/// all been taken holds the session; other calls wait until its
/// [`QueryHandle`] is drained or dropped.
#[async_trait]
pub trait Session: Send + Sync {
    /// Run one statement. Returns once the database has accepted it and its
    /// columns are known; rows then arrive in pages through the handle.
    ///
    /// # Errors
    ///
    /// [`Error::Query`](crate::Error::Query) when the database rejects the
    /// statement, with a position when it reports one. Errors that happen
    /// while rows are streaming arrive through [`QueryHandle::pages`].
    async fn execute(&self, sql: &str, params: &[Value]) -> Result<QueryHandle>;

    /// A handle that cancels whatever this session is running.
    fn canceller(&self) -> Canceller;

    /// List schemas and their tables and views.
    ///
    /// # Errors
    ///
    /// Any error the catalog queries raise.
    async fn catalog(&self) -> Result<Catalog>;

    /// Describe one table's columns, keys and indexes.
    ///
    /// # Errors
    ///
    /// [`Error::Query`](crate::Error::Query) when the table does not exist.
    async fn table_detail(&self, table: &TableRef) -> Result<TableDetail>;

    /// Run a change set in one transaction, all or nothing.
    ///
    /// Every statement must affect exactly one row. When one does not — the
    /// row moved or went away between being read and being applied — the whole
    /// transaction rolls back and the error is
    /// [`Error::Stale`](crate::Error::Stale).
    ///
    /// # Errors
    ///
    /// [`Error::Stale`](crate::Error::Stale) on a key mismatch,
    /// [`Error::Query`](crate::Error::Query) when the database rejects a
    /// statement, [`Error::Closed`](crate::Error::Closed) when the session is
    /// gone.
    async fn apply(&self, statements: &[EditStatement]) -> Result<()>;

    /// The databases on the server, for kinds where a connection opens one
    /// of several (see [`DbKind::lists_databases`]). Empty for the others.
    ///
    /// # Errors
    ///
    /// Any error the listing statement raises.
    async fn databases(&self) -> Result<Vec<String>> {
        let sql = match self.dialect().kind() {
            DbKind::Postgres => {
                "SELECT datname FROM pg_database WHERE datallowconn AND NOT datistemplate \
                 ORDER BY datname"
            }
            DbKind::SqlServer => "SELECT name FROM sys.databases WHERE state = 0 ORDER BY name",
            DbKind::Snowflake => "SHOW TERSE DATABASES",
            DbKind::Redis => "INFO keyspace",
            _ => return Ok(Vec::new()),
        };
        let mut handle = self.execute(sql, &[]).await?;
        // The name is in the column called `name`, or `key` for Redis's
        // `db0:keys=...` lines; otherwise the first one.
        let column = handle
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case("key"))
            .or_else(|| {
                handle
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case("name"))
            })
            .unwrap_or(0);
        let mut names = Vec::new();
        while let Some(page) = futures::StreamExt::next(&mut handle.pages).await {
            for row in page?.rows {
                if let Some(value) = row.get(column).filter(|v| !v.is_null()) {
                    names.push(value.to_string());
                }
            }
        }
        Ok(names)
    }

    /// The SQL spelling differences for this database.
    fn dialect(&self) -> &'static dyn Dialect;

    /// Close the connection.
    ///
    /// # Errors
    ///
    /// [`Error::Closed`](crate::Error::Closed) when it was already gone.
    async fn close(self: Box<Self>) -> Result<()>;
}

/// A running statement.
#[derive(Debug)]
pub struct QueryHandle {
    /// The result's columns. Empty for statements that return no rows.
    pub columns: Vec<ColumnMeta>,
    /// Pages of up to [`PAGE_ROWS`] rows. The channel is bounded, so a
    /// consumer that stops taking pages stops the query reading from the
    /// database. Dropping the receiver cancels the statement. An `Err` is
    /// always the last item.
    pub pages: mpsc::Receiver<Result<ResultPage>>,
    /// Resolves after the last page when the statement succeeded. Cancelled
    /// (dropped) when it failed.
    pub summary: oneshot::Receiver<QuerySummary>,
}

/// What a finished statement reports.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QuerySummary {
    /// Rows changed, for statements that change rows.
    pub rows_affected: Option<u64>,
    /// Rows returned through the pages.
    pub rows_returned: u64,
    /// Wall time from sending the statement to its last row.
    pub elapsed: Duration,
    /// Notices and warnings the database raised while it ran.
    pub notices: Vec<String>,
}

/// Cancels a session's running statement. Cheap to clone and safe to call
/// from any thread; cancelling an idle session does nothing.
#[derive(Clone)]
pub struct Canceller(Arc<dyn Fn() + Send + Sync>);

impl Canceller {
    pub(crate) fn new(cancel: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Arc::new(cancel))
    }

    /// Ask the database to stop the running statement. Returns at once; the
    /// statement then ends with [`Error::Cancelled`](crate::Error::Cancelled).
    pub fn cancel(&self) {
        (self.0)();
    }
}

impl fmt::Debug for Canceller {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Canceller")
    }
}

/// The small differences that leak into generated SQL.
pub trait Dialect: Send + Sync {
    /// The database this dialect is for.
    fn kind(&self) -> DbKind;

    /// The placeholder for the `n`th parameter, counting from 1.
    fn placeholder(&self, n: usize) -> String;

    /// Quote an identifier so it is taken literally.
    fn quote_ident(&self, ident: &str) -> String {
        format!("\"{}\"", ident.replace('"', "\"\""))
    }

    /// A table's name, quoted and qualified with its schema when it has one.
    fn quote_table(&self, table: &TableRef) -> String {
        match &table.schema {
            Some(schema) => format!(
                "{}.{}",
                self.quote_ident(schema),
                self.quote_ident(&table.name)
            ),
            None => self.quote_ident(&table.name),
        }
    }

    /// A statement that adds a row of nothing but default values.
    fn insert_defaults(&self, table: &TableRef) -> String {
        format!("INSERT INTO {} DEFAULT VALUES", self.quote_table(table))
    }

    /// The clause that pages a `SELECT`.
    fn limit_clause(&self, limit: u64, offset: u64) -> String {
        format!("LIMIT {limit} OFFSET {offset}")
    }

    /// The statement a table view runs: `columns` of `table`, narrowed by
    /// the filter box's text and sorted on one column, ascending when the
    /// flag is set. The rows must come back in `columns` order.
    fn browse(
        &self,
        table: &TableRef,
        columns: &[String],
        filter: &str,
        sort: Option<(&str, bool)>,
    ) -> String {
        let names: Vec<String> = columns.iter().map(|c| self.quote_ident(c)).collect();
        let mut sql = format!(
            "SELECT {} FROM {}",
            names.join(", "),
            self.quote_table(table)
        );
        let filter = filter.trim();
        if !filter.is_empty() {
            // On lines of its own, so a trailing comment cannot swallow the
            // closing parenthesis, and parenthesised so an OR stays inside.
            sql.push_str(&format!(" WHERE (\n{filter}\n)"));
        }
        if let Some((column, ascending)) = sort {
            let direction = if ascending { "ASC" } else { "DESC" };
            sql.push_str(&format!(
                " ORDER BY {} {direction}",
                self.quote_ident(column)
            ));
        }
        sql
    }

    /// Whether [`browse`](Self::browse) can sort on any column.
    fn sorts(&self) -> bool {
        true
    }

    /// The table view's filter box placeholder.
    fn filter_hint(&self) -> &'static str {
        "Filter rows with a WHERE condition, such as id > 10"
    }

    /// Why a table view cannot edit this database's rows, or `None` when it
    /// can.
    fn read_only_reason(&self) -> Option<&'static str> {
        None
    }

    /// The statement that adds a row. See [`crate::edit::insert`].
    fn insert(&self, table: &TableRef, columns: &[&str], values: &[Value]) -> EditStatement {
        crate::edit::sql_insert(self, table, columns, values)
    }

    /// The statement that changes one row. See [`crate::edit::update`].
    fn update(
        &self,
        table: &TableRef,
        key_columns: &[&str],
        key: &[Value],
        columns: &[&str],
        values: &[Value],
    ) -> EditStatement {
        crate::edit::sql_update(self, table, key_columns, key, columns, values)
    }

    /// The statement that removes one row. See [`crate::edit::delete`].
    fn delete(&self, table: &TableRef, key_columns: &[&str], key: &[Value]) -> EditStatement {
        crate::edit::sql_delete(self, table, key_columns, key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_debug_should_not_show_the_password() {
        let shown = format!("{:?}", Secret::new("hunter2"));

        assert!(!shown.contains("hunter2"));
    }

    #[test]
    fn quote_ident_should_double_embedded_quotes() {
        let quoted = crate::sqlite::SqliteDialect.quote_ident("we\"ird");

        assert_eq!(quoted, "\"we\"\"ird\"");
    }

    #[test]
    fn quote_table_should_qualify_with_schema() {
        let table = TableRef::new("public", "Order Items");

        assert_eq!(
            crate::postgres::PostgresDialect.quote_table(&table),
            "\"public\".\"Order Items\""
        );
    }
}
