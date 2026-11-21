//! The error type shared by every driver.

use thiserror::Error;

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// What can go wrong talking to a database.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum Error {
    /// The connection could not be established or authenticated.
    #[error("could not connect: {0}")]
    Connect(String),

    /// The database rejected a statement.
    #[error("{message}")]
    Query {
        /// The database's own message.
        message: String,
        /// Byte offset into the statement text where the problem is, when the
        /// database reports one.
        position: Option<usize>,
        /// `SQLSTATE` for Postgres, the extended result code name for SQLite.
        code: Option<String>,
        /// Extra explanation from the database, if any.
        detail: Option<String>,
        /// The database's suggestion for fixing it, if any.
        hint: Option<String>,
    },

    /// The statement was cancelled before it finished.
    #[error("query cancelled")]
    Cancelled,

    /// A change set rolled back because one of its rows no longer matched the
    /// key it was read with. Nothing was written.
    #[error("a row changed or disappeared while it was being edited; nothing was written")]
    Stale,

    /// The session is closed, or its connection dropped.
    #[error("the connection to the database was lost")]
    Closed,

    /// A value could not be sent as a parameter of the type the database
    /// expects.
    #[error("cannot send {value} as {expected}")]
    Parameter {
        /// The kind of value offered.
        value: &'static str,
        /// The database's type name for the parameter.
        expected: String,
    },

    /// The configuration asks for something that cannot be done.
    #[error("invalid configuration: {0}")]
    Config(String),
}

impl Error {
    /// A query error carrying only a message.
    pub(crate) fn query(message: impl Into<String>) -> Self {
        Self::Query {
            message: message.into(),
            position: None,
            code: None,
            detail: None,
            hint: None,
        }
    }

    /// Whether the session is gone and a new one has to be opened before
    /// anything else can run.
    pub fn is_connection_lost(&self) -> bool {
        matches!(self, Self::Closed)
    }

    /// Byte offset of the problem in the statement text, for query errors that
    /// have one.
    pub fn position(&self) -> Option<usize> {
        match self {
            Self::Query { position, .. } => *position,
            _ => None,
        }
    }
}
