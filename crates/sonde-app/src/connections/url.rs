//! Import the connection settings Sonde can actually represent.

use percent_encoding::percent_decode_str;
use sonde_core::{ConnectionConfig, DbKind, Secret, ServerConfig, SqliteConfig, TlsMode};
use url::{Host, Url};

pub(super) struct ImportedConnection {
    pub name: String,
    pub config: ConnectionConfig,
    pub password: Option<Secret>,
}

fn decode(value: &str) -> Result<String, &'static str> {
    percent_decode_str(value)
        .decode_utf8()
        .map(|value| value.into_owned())
        .map_err(|_| "The URL contains text that is not valid UTF-8.")
}

pub(super) fn parse(value: &str) -> Result<ImportedConnection, &'static str> {
    // Never return the parser's error: it could include a password from the URL.
    let url = Url::parse(value.trim())
        .map_err(|_| "Enter a valid Postgres, MySQL, MariaDB, MongoDB, Redis or SQLite URL.")?;
    if url.fragment().is_some() {
        return Err("Remove the URL fragment. Encode a literal # as %23.");
    }
    match url.scheme() {
        scheme @ ("postgres" | "postgresql" | "mysql" | "mariadb" | "redis" | "rediss"
        | "valkey" | "valkeys" | "mongodb" | "mongodb+srv") => {
            let kind = match scheme {
                "postgres" | "postgresql" => DbKind::Postgres,
                "mysql" => DbKind::Mysql,
                "mariadb" => DbKind::Mariadb,
                "mongodb" | "mongodb+srv" => DbKind::Mongodb,
                _ => DbKind::Redis,
            };
            let host = match url.host() {
                Some(Host::Domain(host)) => host.to_owned(),
                Some(Host::Ipv4(host)) => host.to_string(),
                Some(Host::Ipv6(host)) => host.to_string(),
                None => return Err("The URL needs a host."),
            };
            let host_name = host.clone();
            let user = decode(url.username())?;
            if user.is_empty() && !kind.user_optional() {
                return Err("The URL needs a user.");
            }
            let database = decode(url.path().strip_prefix('/').unwrap_or(url.path()))?;
            // Postgres falls back to a database named after the user; MySQL
            // can connect to none at all.
            let database = if database.is_empty() && kind == DbKind::Postgres {
                user.clone()
            } else {
                database
            };
            let mut tls = if matches!(scheme, "rediss" | "valkeys") {
                TlsMode::Require
            } else {
                TlsMode::Prefer
            };
            let mut options = std::collections::BTreeMap::new();
            if kind == DbKind::Mongodb {
                // MongoDB's options are many; they are kept as written and
                // handed to the driver.
                if scheme == "mongodb+srv" {
                    options.insert("srv".to_owned(), "true".to_owned());
                }
                if let Some(query) = url.query().filter(|q| !q.is_empty()) {
                    options.insert("options".to_owned(), query.to_owned());
                }
            }
            if kind == DbKind::Redis && !database.is_empty() && database.parse::<u32>().is_err() {
                return Err("A Redis URL's path is a database number, such as /0.");
            }
            let pairs: Vec<(String, String)> = if kind == DbKind::Mongodb {
                Vec::new()
            } else {
                url.query_pairs()
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect()
            };
            for (key, value) in pairs {
                if !matches!(key.as_str(), "sslmode" | "ssl-mode") {
                    return Err(
                        "Only the sslmode URL option is supported. Enter other settings manually.",
                    );
                }
                tls = match value.to_ascii_lowercase().as_str() {
                    "disable" | "disabled" => TlsMode::Disable,
                    "prefer" | "preferred" => TlsMode::Prefer,
                    "require" | "required" => TlsMode::Require,
                    "verify-full" | "verify_identity" => TlsMode::VerifyFull,
                    _ => return Err("Use sslmode=disable, prefer, require, or verify-full."),
                };
            }
            let server = ServerConfig {
                host,
                port: url.port().or(kind.default_port()).unwrap_or(5432),
                database: database.clone(),
                user: user.clone(),
                tls,
                options,
            };
            Ok(ImportedConnection {
                name: if !database.is_empty() && kind != DbKind::Redis {
                    database
                } else if !user.is_empty() {
                    user
                } else {
                    host_name.clone()
                },
                config: ConnectionConfig::for_server(kind, server)
                    .ok_or("That database is not a server.")?,
                password: url.password().map(decode).transpose()?.map(Secret::new),
            })
        }
        "sqlite" => {
            if url.host().is_some() || !url.username().is_empty() || url.password().is_some() {
                return Err("Use sqlite:///absolute/path/to/database.db for a local file.");
            }
            let path = decode(url.path())?;
            if !path.starts_with('/') || path == "/" {
                return Err("The SQLite URL needs an absolute file path.");
            }
            let mut read_only = false;
            for (key, value) in url.query_pairs() {
                match (key.as_ref(), value.as_ref()) {
                    ("mode", "ro") => read_only = true,
                    ("mode", "rwc") => read_only = false,
                    _ => return Err("SQLite URLs support mode=ro or mode=rwc."),
                }
            }
            let path = std::path::PathBuf::from(path);
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("SQLite")
                .to_owned();
            Ok(ImportedConnection {
                name,
                config: ConnectionConfig::Sqlite(SqliteConfig { path, read_only }),
                password: None,
            })
        }
        _ => Err("Sonde imports Postgres, MySQL, MariaDB, MongoDB, Redis and SQLite URLs."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postgres_should_decode_credentials_and_preserve_tls() {
        let imported =
            parse("postgresql://a%40b:p%23ss@[::1]:5433/my%20db?sslmode=verify-full").unwrap();
        let ConnectionConfig::Postgres(config) = imported.config else {
            panic!("expected Postgres")
        };
        assert_eq!(
            (
                config.host.as_str(),
                config.port,
                config.user.as_str(),
                config.database.as_str(),
                config.tls
            ),
            ("::1", 5433, "a@b", "my db", TlsMode::VerifyFull)
        );
        assert_eq!(imported.password.unwrap().expose(), "p#ss");
    }

    #[test]
    fn mysql_should_import_with_its_default_port() {
        let imported = parse("mysql://root:pw@db.local/shop?ssl-mode=REQUIRED").unwrap();
        let ConnectionConfig::Mysql(config) = imported.config else {
            panic!("expected MySQL")
        };
        assert_eq!(
            (config.port, config.database.as_str(), config.tls),
            (3306, "shop", TlsMode::Require)
        );
        assert_eq!(imported.password.unwrap().expose(), "pw");
    }

    #[test]
    fn postgres_should_default_database_to_user() {
        let imported = parse("postgres://alice@localhost").unwrap();
        assert_eq!(imported.name, "alice");
    }

    #[test]
    fn sqlite_should_decode_local_read_only_path() {
        let imported = parse("sqlite:///tmp/my%20data.db?mode=ro").unwrap();
        let ConnectionConfig::Sqlite(config) = imported.config else {
            panic!("expected SQLite")
        };
        assert!(config.read_only && config.path == std::path::Path::new("/tmp/my data.db"));
    }

    #[test]
    fn urls_should_reject_settings_that_would_be_lost() {
        for input in [
            "mssql://alice@localhost/db",
            "postgres://alice@localhost/db?options=secret",
            "postgres://alice@localhost/db?sslmode=verify-ca",
            "sqlite://server/db",
            "postgres://alice@localhost/db#fragment",
        ] {
            assert!(parse(input).is_err(), "{input}");
        }
    }

    #[test]
    fn redis_and_mongodb_urls_should_import() {
        let redis = parse("rediss://:secret@cache.example:6380/2").unwrap();
        let ConnectionConfig::Redis(config) = &redis.config else {
            panic!("a Redis connection");
        };
        assert_eq!((config.port, config.database.as_str()), (6380, "2"));
        assert_eq!(config.tls, TlsMode::Require);
        assert_eq!(redis.password.unwrap().expose(), "secret");

        let mongo =
            parse("mongodb+srv://ada@cluster0.example/shop?retryWrites=true&w=majority").unwrap();
        let ConnectionConfig::Mongodb(config) = &mongo.config else {
            panic!("a MongoDB connection");
        };
        assert_eq!(config.option("srv"), Some("true"));
        assert_eq!(
            config.option("options"),
            Some("retryWrites=true&w=majority")
        );
        assert_eq!(mongo.name, "shop");

        assert!(matches!(
            parse("mariadb://u@h/db").unwrap().config,
            ConnectionConfig::Mariadb(_)
        ));
    }
}
