//! Completion for the languages that are not SQL: Redis commands and their
//! keys, and `mongosh` calls with collection and method names.

use super::{Analysis, Candidate, CandidateKind, LIMIT};
use crate::schema::Catalog;

/// Redis commands offered at the start of a line.
const REDIS_COMMANDS: &[&str] = &[
    "APPEND",
    "BITCOUNT",
    "BLPOP",
    "BRPOP",
    "CLIENT",
    "CONFIG",
    "COPY",
    "DBSIZE",
    "DECR",
    "DECRBY",
    "DEL",
    "EVAL",
    "EXISTS",
    "EXPIRE",
    "EXPIREAT",
    "FLUSHDB",
    "GET",
    "GETDEL",
    "GETEX",
    "GETRANGE",
    "HDEL",
    "HEXISTS",
    "HGET",
    "HGETALL",
    "HINCRBY",
    "HKEYS",
    "HLEN",
    "HMGET",
    "HSCAN",
    "HSET",
    "HSETNX",
    "HVALS",
    "INCR",
    "INCRBY",
    "INCRBYFLOAT",
    "INFO",
    "JSON.GET",
    "JSON.SET",
    "KEYS",
    "LINDEX",
    "LINSERT",
    "LLEN",
    "LPOP",
    "LPUSH",
    "LRANGE",
    "LREM",
    "LSET",
    "LTRIM",
    "MEMORY",
    "MGET",
    "MSET",
    "OBJECT",
    "PERSIST",
    "PEXPIRE",
    "PING",
    "PTTL",
    "RENAME",
    "RPOP",
    "RPUSH",
    "SADD",
    "SCAN",
    "SCARD",
    "SDIFF",
    "SELECT",
    "SET",
    "SETEX",
    "SETNX",
    "SINTER",
    "SISMEMBER",
    "SLOWLOG",
    "SMEMBERS",
    "SMOVE",
    "SPOP",
    "SREM",
    "SSCAN",
    "STRLEN",
    "SUNION",
    "TIME",
    "TTL",
    "TYPE",
    "UNLINK",
    "XADD",
    "XDEL",
    "XINFO",
    "XLEN",
    "XRANGE",
    "XREVRANGE",
    "XTRIM",
    "ZADD",
    "ZCARD",
    "ZCOUNT",
    "ZINCRBY",
    "ZRANGE",
    "ZRANGEBYSCORE",
    "ZRANK",
    "ZREM",
    "ZREVRANGE",
    "ZSCAN",
    "ZSCORE",
];

/// Commands whose first argument is not a key.
const NO_KEY: &[&str] = &[
    "CLIENT", "CONFIG", "DBSIZE", "EVAL", "FLUSHDB", "INFO", "KEYS", "MEMORY", "OBJECT", "PING",
    "SCAN", "SELECT", "SLOWLOG", "TIME",
];

const DB_METHODS: &[&str] = &[
    "getCollection",
    "getSiblingDB",
    "runCommand",
    "adminCommand",
    "getCollectionNames",
    "createCollection",
    "dropDatabase",
    "stats",
];

const COLLECTION_METHODS: &[&str] = &[
    "find",
    "findOne",
    "aggregate",
    "countDocuments",
    "estimatedDocumentCount",
    "distinct",
    "insertOne",
    "insertMany",
    "updateOne",
    "updateMany",
    "replaceOne",
    "deleteOne",
    "deleteMany",
    "getIndexes",
    "createIndex",
    "dropIndex",
    "drop",
];

const CURSOR_METHODS: &[&str] = &["sort", "limit", "skip", "projection", "batchSize", "hint"];

const OPERATORS: &[&str] = &[
    "$and",
    "$or",
    "$nor",
    "$not",
    "$eq",
    "$ne",
    "$gt",
    "$gte",
    "$lt",
    "$lte",
    "$in",
    "$nin",
    "$exists",
    "$type",
    "$regex",
    "$elemMatch",
    "$size",
    "$all",
    "$expr",
    "$set",
    "$unset",
    "$inc",
    "$push",
    "$pull",
    "$addToSet",
    "$match",
    "$group",
    "$project",
    "$sort",
    "$limit",
    "$skip",
    "$lookup",
    "$unwind",
    "$count",
    "$sum",
    "$avg",
    "$min",
    "$max",
    "$first",
    "$last",
    "$addFields",
    "$facet",
];

/// The byte range of the word around `cursor`, where a word is anything
/// that `is_word` accepts.
fn word_at(sql: &str, cursor: usize, is_word: impl Fn(char) -> bool) -> (usize, usize) {
    let start = sql[..cursor]
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_word(*c))
        .last()
        .map_or(cursor, |(ix, _)| ix);
    let end = sql[cursor..]
        .char_indices()
        .find(|(_, c)| !is_word(*c))
        .map_or(sql.len(), |(ix, _)| cursor + ix);
    (start, end)
}

fn analysis(
    sql: &str,
    start: usize,
    end: usize,
    cursor: usize,
    qualifier: Vec<String>,
) -> Analysis {
    Analysis {
        replace: start..end,
        prefix: sql[start..cursor].to_owned(),
        qualifier,
        mentions: Vec::new(),
        wants_table: true,
    }
}

/// The command name, or a key after a command that takes one.
pub(super) fn redis(sql: &str, cursor: usize, explicit: bool) -> Option<Analysis> {
    let line_start = sql[..cursor].rfind('\n').map_or(0, |ix| ix + 1);
    let before = &sql[line_start..cursor];
    if before.matches('"').count() % 2 == 1 || before.matches('\'').count() % 2 == 1 {
        return None;
    }
    let (start, end) = word_at(sql, cursor, |c| !c.is_whitespace() && c != '"' && c != '\'');
    if start == cursor && !explicit {
        return None;
    }
    let words: Vec<&str> = sql[line_start..start].split_whitespace().collect();
    match words.as_slice() {
        [] => Some(analysis(sql, start, end, cursor, Vec::new())),
        [command] if !NO_KEY.contains(&command.to_ascii_uppercase().as_str()) => Some(analysis(
            sql,
            start,
            end,
            cursor,
            vec![command.to_ascii_uppercase()],
        )),
        _ => None,
    }
}

pub(super) fn redis_candidates(analysis: &Analysis, catalog: &Catalog) -> Vec<Candidate> {
    let prefix = analysis.prefix.to_lowercase();
    if analysis.qualifier.is_empty() {
        return REDIS_COMMANDS
            .iter()
            .filter(|name| name.to_lowercase().starts_with(&prefix))
            .take(LIMIT)
            .map(|name| Candidate {
                label: (*name).to_owned(),
                insert: (*name).to_owned(),
                kind: CandidateKind::Keyword,
                detail: None,
            })
            .collect();
    }
    catalog
        .schemas
        .iter()
        .flat_map(|schema| &schema.relations)
        .filter(|key| key.name.to_lowercase().starts_with(&prefix))
        .take(LIMIT)
        .map(|key| Candidate {
            label: key.name.clone(),
            insert: crate::kv::quote_arg(&key.name),
            kind: CandidateKind::Table,
            detail: Some("key".into()),
        })
        .collect()
}

/// `db.<collection>`, `db.<collection>.<method>`, a cursor method after a
/// call, or a `$` operator inside the arguments.
pub(super) fn mongo(sql: &str, cursor: usize, explicit: bool) -> Option<Analysis> {
    let is_word = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    let (start, end) = word_at(sql, cursor, is_word);
    let before = &sql[..start];
    if before.matches('"').count() % 2 == 1 || before.matches('\'').count() % 2 == 1 {
        return None;
    }
    if sql[start..cursor].starts_with('$') {
        return Some(analysis(sql, start, end, cursor, vec!["$".into()]));
    }
    let dotted = before.strip_suffix('.')?;
    if start == cursor && !explicit && !dotted.ends_with(|c: char| is_word(c) || c == ')') {
        return None;
    }
    if dotted.ends_with(')') {
        return Some(analysis(sql, start, end, cursor, vec![")".into()]));
    }
    let (word_start, _) = word_at(dotted, dotted.len(), |c| is_word(c) || c == '-');
    let word = &dotted[word_start..];
    let ahead = &dotted[..word_start];
    if word == "db" && !ahead.ends_with('.') {
        return Some(analysis(sql, start, end, cursor, vec!["db".into()]));
    }
    if ahead.ends_with("db.") || ahead.ends_with(").") {
        return Some(analysis(
            sql,
            start,
            end,
            cursor,
            vec!["db".into(), word.to_owned()],
        ));
    }
    None
}

pub(super) fn mongo_candidates(analysis: &Analysis, catalog: &Catalog) -> Vec<Candidate> {
    let prefix = analysis.prefix.to_lowercase();
    let fits = |name: &str| name.to_lowercase().starts_with(&prefix);
    let words = |names: &[&str], kind: CandidateKind| -> Vec<Candidate> {
        names
            .iter()
            .filter(|name| fits(name))
            .map(|name| Candidate {
                label: (*name).to_owned(),
                insert: (*name).to_owned(),
                kind,
                detail: None,
            })
            .collect()
    };
    let mut out = match analysis.qualifier.as_slice() {
        [only] if only == "$" => words(OPERATORS, CandidateKind::Keyword),
        [only] if only == ")" => words(CURSOR_METHODS, CandidateKind::Keyword),
        [only] if only == "db" => {
            // The current database is listed first.
            let mut out: Vec<Candidate> = catalog
                .schemas
                .first()
                .into_iter()
                .flat_map(|schema| &schema.relations)
                .filter(|relation| fits(&relation.name))
                .map(|relation| Candidate {
                    label: relation.name.clone(),
                    insert: relation.name.clone(),
                    kind: CandidateKind::Table,
                    detail: catalog.schemas.first().map(|s| s.name.clone()),
                })
                .collect();
            out.extend(words(DB_METHODS, CandidateKind::Keyword));
            out
        }
        [_, _] => words(COLLECTION_METHODS, CandidateKind::Keyword),
        _ => Vec::new(),
    };
    out.truncate(LIMIT);
    out
}

/// Whether the cursor is inside a REST request rather than SQL.
pub(super) fn in_request(sql: &str, cursor: usize) -> bool {
    crate::split::split(sql, crate::DbKind::Elasticsearch)
        .iter()
        .find(|statement| {
            statement.offset <= cursor && cursor <= statement.offset + statement.sql.len()
        })
        .is_some_and(|statement| {
            crate::split::is_request_line(statement.sql.lines().next().unwrap_or(""))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Relation, RelationKind, Schema};

    fn catalog() -> Catalog {
        Catalog {
            schemas: vec![Schema {
                name: "shop".into(),
                relations: vec![
                    Relation {
                        name: "orders".into(),
                        kind: RelationKind::Collection,
                    },
                    Relation {
                        name: "users".into(),
                        kind: RelationKind::Collection,
                    },
                ],
            }],
        }
    }

    fn labels(candidates: Vec<Candidate>) -> Vec<String> {
        candidates.into_iter().map(|c| c.label).collect()
    }

    #[test]
    fn redis_should_offer_commands_then_keys() {
        let commands = redis("hge", 3, false).unwrap();
        assert!(labels(redis_candidates(&commands, &catalog())).contains(&"HGETALL".into()));

        let key = redis("HGETALL us", 10, false).unwrap();
        assert_eq!(labels(redis_candidates(&key, &catalog())), ["users"]);
        assert!(redis("INFO ser", 8, false).is_none());
    }

    #[test]
    fn mongo_should_offer_collections_then_methods() {
        let sql = "db.or";
        let collections = mongo(sql, sql.len(), false).unwrap();
        assert_eq!(
            labels(mongo_candidates(&collections, &catalog())),
            ["orders"]
        );

        let sql = "db.orders.fi";
        let methods = mongo(sql, sql.len(), false).unwrap();
        assert_eq!(
            labels(mongo_candidates(&methods, &catalog())),
            ["find", "findOne"]
        );

        let sql = "db.orders.find({}).so";
        let chained = mongo(sql, sql.len(), false).unwrap();
        assert_eq!(labels(mongo_candidates(&chained, &catalog())), ["sort"]);

        let sql = "db.orders.find({total: {$g";
        let operator = mongo(sql, sql.len(), false).unwrap();
        assert_eq!(
            labels(mongo_candidates(&operator, &catalog())),
            ["$gt", "$gte", "$group"]
        );
    }

    #[test]
    fn elastic_requests_should_not_complete_as_sql() {
        let sql = "SELECT 1;\nGET /logs/_search\n{\"query\": {}}";
        assert!(in_request(sql, sql.len() - 3));
        assert!(!in_request(sql, 3));
    }
}
