//! Filters built from choices rather than typed: rules of the form
//! *column, condition, value*, joined by "all" or "any", which each
//! database turns into its own condition.
//!
//! The result is the text a table view's filter box takes, so a built
//! filter and a typed one take the same path: a `WHERE` condition for the
//! SQL databases and Cassandra, a filter document for MongoDB.

use serde::{Deserialize, Serialize};

use crate::{
    driver::{DbKind, Dialect},
    schema::TableRef,
};

/// What kind of values a column holds, which decides the conditions it is
/// offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Strings, and anything without a better class.
    Text,
    /// Integers and decimals.
    Number,
    /// Dates and timestamps.
    Date,
    /// True or false.
    Bool,
    /// JSON documents, arrays and other nested values.
    Nested,
}

impl Class {
    /// The class of a column from its type as the database writes it.
    pub fn of(type_name: &str) -> Self {
        let name = type_name.trim().to_ascii_lowercase();
        let base = name.split(['(', ' ', '<']).next().unwrap_or_default();
        if matches!(base, "bool" | "boolean" | "bit") || name == "tinyint(1)" {
            return Self::Bool;
        }
        if matches!(
            base,
            "json" | "jsonb" | "object" | "array" | "variant" | "nested" | "rejson-rl" | "fields"
        ) || base.starts_with("map")
            || base.starts_with("set")
            || base.starts_with("list")
            || base.starts_with("frozen")
            || base.starts_with('_')
            || name.ends_with("[]")
        {
            return Self::Nested;
        }
        if base.contains("date") || base.starts_with("timestamp") || base == "smalldatetime" {
            return Self::Date;
        }
        let numeric = [
            "int", "serial", "decimal", "numeric", "real", "double", "float", "money", "number",
            "counter", "long", "short", "byte",
        ];
        if numeric.iter().any(|n| base.contains(n)) && !base.contains("interval") {
            return Self::Number;
        }
        Self::Text
    }
}

/// A condition a rule puts on its column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Op {
    /// Equal to the value. On a date column, a date alone means that day.
    Is,
    /// Not equal to the value.
    IsNot,
    /// Holds the value anywhere, ignoring case where the database can.
    Contains,
    /// Does not hold the value.
    NotContains,
    /// Starts with the value.
    StartsWith,
    /// Ends with the value.
    EndsWith,
    /// Full-text match (Elasticsearch).
    Matches,
    /// Greater than the value.
    Greater,
    /// Greater than or equal to the value.
    AtLeast,
    /// Less than the value.
    Less,
    /// Less than or equal to the value.
    AtMost,
    /// Between the two values, both included.
    Between,
    /// Before the value (dates).
    Before,
    /// After the value (dates).
    After,
    /// Within the last so many days (dates).
    InLastDays,
    /// Equal to one of a comma-separated list.
    OneOf,
    /// True.
    IsTrue,
    /// False.
    IsFalse,
    /// `NULL`, or missing from the document.
    IsEmpty,
    /// Not `NULL`.
    IsNotEmpty,
}

impl Op {
    /// The condition as the builder lists it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Is => "is",
            Self::IsNot => "is not",
            Self::Contains => "contains",
            Self::NotContains => "does not contain",
            Self::StartsWith => "starts with",
            Self::EndsWith => "ends with",
            Self::Matches => "matches words",
            Self::Greater => "greater than",
            Self::AtLeast => "at least",
            Self::Less => "less than",
            Self::AtMost => "at most",
            Self::Between => "between",
            Self::Before => "before",
            Self::After => "after",
            Self::InLastDays => "in the last (days)",
            Self::OneOf => "is one of",
            Self::IsTrue => "is true",
            Self::IsFalse => "is false",
            Self::IsEmpty => "is empty",
            Self::IsNotEmpty => "is not empty",
        }
    }

    /// How many values the condition takes: none, one, or two for
    /// [`Op::Between`].
    pub fn values(self) -> usize {
        match self {
            Self::IsTrue | Self::IsFalse | Self::IsEmpty | Self::IsNotEmpty => 0,
            Self::Between => 2,
            _ => 1,
        }
    }

    /// What to put in an empty value box.
    pub fn placeholder(self, class: Class) -> &'static str {
        match (self, class) {
            (Self::OneOf, _) => "a, b, c",
            (Self::InLastDays, _) => "30",
            (_, Class::Date) => "2024-01-31",
            (_, Class::Number) => "0",
            _ => "value",
        }
    }
}

/// The conditions a column of `class` is offered on `kind`, most used
/// first.
pub fn ops(kind: DbKind, class: Class) -> Vec<Op> {
    use Op::*;
    if kind == DbKind::Cassandra {
        // CQL compares and lists; it has no LIKE, no NULL test and no OR.
        return match class {
            Class::Bool => vec![IsTrue, IsFalse],
            Class::Nested => Vec::new(),
            Class::Date => vec![Is, Before, After, Between, InLastDays, OneOf],
            _ => vec![Is, Greater, AtLeast, Less, AtMost, Between, OneOf],
        };
    }
    let mut ops = match class {
        Class::Text => vec![
            Is,
            IsNot,
            Contains,
            NotContains,
            StartsWith,
            EndsWith,
            OneOf,
            IsEmpty,
            IsNotEmpty,
        ],
        Class::Number => vec![
            Is, IsNot, Greater, AtLeast, Less, AtMost, Between, OneOf, IsEmpty, IsNotEmpty,
        ],
        Class::Date => vec![Is, Before, After, Between, InLastDays, IsEmpty, IsNotEmpty],
        Class::Bool => vec![IsTrue, IsFalse, IsEmpty, IsNotEmpty],
        Class::Nested => vec![IsEmpty, IsNotEmpty],
    };
    if kind == DbKind::Elasticsearch && class == Class::Text {
        ops.insert(0, Matches);
    }
    ops
}

/// Whether `kind` can join rules with "any". Cassandra has no `OR`.
pub fn supports_any(kind: DbKind) -> bool {
    kind != DbKind::Cassandra
}

/// Whether the builder works for `kind` at all. Redis keys are filtered by
/// pattern instead.
pub fn supported(kind: DbKind) -> bool {
    kind != DbKind::Redis
}

/// The pseudo-column for searching every field (Elasticsearch).
pub const ANY_FIELD: &str = "*";

/// One condition on one column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    /// The column, or [`ANY_FIELD`].
    pub column: String,
    /// The column's type as the database writes it.
    #[serde(default)]
    pub type_name: String,
    /// The condition.
    pub op: Op,
    /// The value, as typed.
    #[serde(default)]
    pub value: String,
    /// The second value, for [`Op::Between`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub value2: String,
}

impl Rule {
    /// A rule with one value.
    pub fn new(column: &str, type_name: &str, op: Op, value: &str) -> Self {
        Self {
            column: column.to_owned(),
            type_name: type_name.to_owned(),
            op,
            value: value.to_owned(),
            value2: String::new(),
        }
    }

    fn class(&self) -> Class {
        Class::of(&self.type_name)
    }
}

/// Rules and how they join.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Filter {
    /// The rules, in the order they were added.
    #[serde(default)]
    pub rules: Vec<Rule>,
    /// Join with "any" (`OR`) rather than "all" (`AND`).
    #[serde(default)]
    pub any: bool,
}

impl Filter {
    /// Whether there are no rules.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

/// The filter as the text the table view's filter box takes on `kind`:
/// a `WHERE` condition, or a MongoDB filter document. Empty for no rules.
///
/// # Errors
///
/// A message naming the rule whose value does not fit, such as a word
/// where a number belongs.
pub fn condition(kind: DbKind, filter: &Filter) -> Result<String, String> {
    if !supported(kind) {
        return Err("Redis keys are filtered with a pattern, such as user:*".into());
    }
    if filter.any && !supports_any(kind) {
        return Err("Cassandra can only match all of the rules, not any of them.".into());
    }
    if kind == DbKind::Mongodb {
        return mongo::condition(filter);
    }
    let sql = Sql {
        kind,
        dialect: crate::driver::dialect(kind),
    };
    let parts = filter
        .rules
        .iter()
        .map(|rule| sql.rule(rule))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(match parts.len() {
        0 => String::new(),
        1 => parts.into_iter().next().unwrap_or_default(),
        _ if filter.any => parts
            .iter()
            .map(|part| format!("({part})"))
            .collect::<Vec<_>>()
            .join(" OR "),
        _ => parts.join(" AND "),
    })
}

/// The time `days` days ago, as `YYYY-MM-DD HH:MM:SS` in UTC.
fn days_ago(days: &str) -> Result<String, String> {
    let days: u32 = days
        .trim()
        .parse()
        .map_err(|_| format!("\"{days}\" is not a number of days"))?;
    let then = chrono::Utc::now() - chrono::Duration::days(i64::from(days));
    Ok(then.format("%Y-%m-%d %H:%M:%S").to_string())
}

/// The day after a `YYYY-MM-DD` date, when `text` is one.
fn next_day(text: &str) -> Option<String> {
    let day = chrono::NaiveDate::parse_from_str(text.trim(), "%Y-%m-%d").ok()?;
    Some(day.succ_opt()?.format("%Y-%m-%d").to_string())
}

fn number(rule: &Rule, text: &str) -> Result<String, String> {
    let text = text.trim();
    text.parse::<f64>()
        .ok()
        .filter(|n| n.is_finite())
        .map(|_| text.to_owned())
        .ok_or_else(|| format!("{} needs a number, not \"{text}\"", rule.column))
}

/// `text` when it starts with a `YYYY-MM-DD` date.
fn checked_date(rule: &Rule, text: &str) -> Result<String, String> {
    let text = text.trim();
    let day = text.get(..10).unwrap_or(text);
    chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d")
        .map(|_| text.to_owned())
        .map_err(|_| {
            format!(
                "{} needs a date such as 2024-01-31, not \"{text}\"",
                rule.column
            )
        })
}

fn list(text: &str) -> Vec<&str> {
    text.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .collect()
}

struct Sql {
    kind: DbKind,
    dialect: &'static dyn Dialect,
}

impl Sql {
    fn string(&self, text: &str) -> String {
        let mut quoted = text.replace('\'', "''");
        if matches!(self.kind, DbKind::Mysql | DbKind::Mariadb) {
            quoted = quoted.replace('\\', "\\\\");
        }
        match self.kind {
            DbKind::SqlServer => format!("N'{quoted}'"),
            _ => format!("'{quoted}'"),
        }
    }

    /// A value for `rule`'s column, typed the way the database compares it.
    fn value(&self, rule: &Rule, text: &str) -> Result<String, String> {
        match rule.class() {
            Class::Number => number(rule, text),
            Class::Date => Ok(self.date(&checked_date(rule, text)?)),
            _ => Ok(self.string(text)),
        }
    }

    fn date(&self, text: &str) -> String {
        match self.kind {
            DbKind::Elasticsearch => format!("CAST({} AS DATETIME)", self.string(text)),
            _ => self.string(text),
        }
    }

    fn boolean(&self, value: bool) -> &'static str {
        match (self.kind, value) {
            (DbKind::SqlServer | DbKind::Sqlite, true) => "1",
            (DbKind::SqlServer | DbKind::Sqlite, false) => "0",
            (_, true) => "TRUE",
            (_, false) => "FALSE",
        }
    }

    /// The column as text, for `LIKE`.
    fn as_text(&self, column: &str) -> String {
        match self.kind {
            DbKind::Postgres => format!("CAST({column} AS text)"),
            DbKind::Snowflake => format!("TO_VARCHAR({column})"),
            _ => column.to_owned(),
        }
    }

    fn like(&self, column: &str, pattern: &str, negate: bool) -> String {
        let not = if negate { "NOT " } else { "" };
        let operator = match self.kind {
            DbKind::Postgres | DbKind::Snowflake => "ILIKE",
            _ => "LIKE",
        };
        if self.kind == DbKind::Elasticsearch {
            // Elasticsearch SQL's LIKE takes no ESCAPE clause.
            return format!("{column} {not}{operator} {}", self.string(pattern));
        }
        format!(
            "{} {not}{operator} {} ESCAPE '!'",
            self.as_text(column),
            self.string(pattern)
        )
    }

    fn rule(&self, rule: &Rule) -> Result<String, String> {
        if rule.column == ANY_FIELD {
            return Ok(format!("QUERY({})", self.string(rule.value.trim())));
        }
        let column = self.dialect.quote_ident(&rule.column);
        let needs = rule.op.values();
        if needs > 0 && rule.value.trim().is_empty() {
            return Err(format!("Give {} a value to compare with.", rule.column));
        }
        if needs > 1 && rule.value2.trim().is_empty() {
            return Err(format!("Give {} a second value for between.", rule.column));
        }
        let escaped = escape_like(&rule.value);
        Ok(match rule.op {
            Op::Is if rule.class() == Class::Date => match next_day(&rule.value) {
                // A day on a timestamp column means the whole day.
                Some(next) => format!(
                    "{column} >= {} AND {column} < {}",
                    self.date(rule.value.trim()),
                    self.date(&next)
                ),
                None => format!("{column} = {}", self.value(rule, &rule.value)?),
            },
            Op::Is => format!("{column} = {}", self.value(rule, &rule.value)?),
            Op::IsNot => format!("{column} <> {}", self.value(rule, &rule.value)?),
            Op::Contains => self.like(&column, &format!("%{escaped}%"), false),
            Op::NotContains => self.like(&column, &format!("%{escaped}%"), true),
            Op::StartsWith => self.like(&column, &format!("{escaped}%"), false),
            Op::EndsWith => self.like(&column, &format!("%{escaped}"), false),
            Op::Matches => format!("MATCH({column}, {})", self.string(rule.value.trim())),
            Op::Greater | Op::After => format!("{column} > {}", self.value(rule, &rule.value)?),
            Op::AtLeast => format!("{column} >= {}", self.value(rule, &rule.value)?),
            Op::Less | Op::Before => format!("{column} < {}", self.value(rule, &rule.value)?),
            Op::AtMost => format!("{column} <= {}", self.value(rule, &rule.value)?),
            Op::Between => format!(
                "{column} >= {} AND {column} <= {}",
                self.value(rule, &rule.value)?,
                self.value(rule, &rule.value2)?
            ),
            Op::InLastDays => format!("{column} >= {}", self.date(&days_ago(&rule.value)?)),
            Op::OneOf => {
                let items = list(&rule.value)
                    .into_iter()
                    .map(|item| self.value(rule, item))
                    .collect::<Result<Vec<_>, _>>()?;
                format!("{column} IN ({})", items.join(", "))
            }
            Op::IsTrue => format!("{column} = {}", self.boolean(true)),
            Op::IsFalse => format!("{column} = {}", self.boolean(false)),
            Op::IsEmpty => format!("{column} IS NULL"),
            Op::IsNotEmpty => format!("{column} IS NOT NULL"),
        })
    }
}

/// `%`, `_` and the escape character itself, escaped with `!` for `LIKE`.
/// SQL Server also treats `[` as special.
fn escape_like(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.trim().chars() {
        if matches!(c, '!' | '%' | '_' | '[') {
            out.push('!');
        }
        out.push(c);
    }
    out
}

mod mongo {
    //! Rules as a MongoDB filter document, in the shell syntax the MongoDB
    //! driver reads.
    use super::{Class, Filter, Op, Rule, checked_date, days_ago, list, next_day, number};

    fn string(text: &str) -> String {
        serde_json::Value::String(text.to_owned()).to_string()
    }

    fn date(text: &str) -> String {
        let text = text.trim();
        let iso = if text.len() == 10 {
            format!("{text}T00:00:00Z")
        } else if text.len() == 19 {
            format!("{}Z", text.replacen(' ', "T", 1))
        } else {
            text.to_owned()
        };
        format!("ISODate({})", string(&iso))
    }

    /// A value typed by the column, or by what it looks like when the
    /// column's type is a guess: documents need not agree.
    fn value(rule: &Rule, text: &str) -> Result<String, String> {
        let text = text.trim();
        match rule.class() {
            Class::Number => number(rule, text),
            Class::Date => Ok(date(&checked_date(rule, text)?)),
            _ if rule.type_name == "objectId" && text.len() == 24 => {
                Ok(format!("ObjectId({})", string(text)))
            }
            _ => Ok(string(text)),
        }
    }

    fn regex(pattern: &str) -> String {
        format!("{{\"$regex\": {}, \"$options\": \"i\"}}", string(pattern))
    }

    fn regex_escape(text: &str) -> String {
        let mut out = String::new();
        for c in text.trim().chars() {
            if "\\^$.|?*+()[]{}".contains(c) {
                out.push('\\');
            }
            out.push(c);
        }
        out
    }

    fn rule(rule: &Rule) -> Result<String, String> {
        let field = string(&rule.column);
        let needs = rule.op.values();
        if needs > 0 && rule.value.trim().is_empty() {
            return Err(format!("Give {} a value to compare with.", rule.column));
        }
        if needs > 1 && rule.value2.trim().is_empty() {
            return Err(format!("Give {} a second value for between.", rule.column));
        }
        let text = regex_escape(&rule.value);
        let condition = match rule.op {
            Op::Is if rule.class() == Class::Date && next_day(&rule.value).is_some() => format!(
                "{{\"$gte\": {}, \"$lt\": {}}}",
                date(&rule.value),
                date(&next_day(&rule.value).unwrap_or_default())
            ),
            Op::Is => value(rule, &rule.value)?,
            Op::IsNot => format!("{{\"$ne\": {}}}", value(rule, &rule.value)?),
            Op::Contains | Op::Matches => regex(&text),
            Op::NotContains => format!("{{\"$not\": {}}}", regex(&text)),
            Op::StartsWith => regex(&format!("^{text}")),
            Op::EndsWith => regex(&format!("{text}$")),
            Op::Greater | Op::After => format!("{{\"$gt\": {}}}", value(rule, &rule.value)?),
            Op::AtLeast => format!("{{\"$gte\": {}}}", value(rule, &rule.value)?),
            Op::Less | Op::Before => format!("{{\"$lt\": {}}}", value(rule, &rule.value)?),
            Op::AtMost => format!("{{\"$lte\": {}}}", value(rule, &rule.value)?),
            Op::Between => format!(
                "{{\"$gte\": {}, \"$lte\": {}}}",
                value(rule, &rule.value)?,
                value(rule, &rule.value2)?
            ),
            Op::InLastDays => format!("{{\"$gte\": {}}}", date(&days_ago(&rule.value)?)),
            Op::OneOf => {
                let items = list(&rule.value)
                    .into_iter()
                    .map(|item| value(rule, item))
                    .collect::<Result<Vec<_>, _>>()?;
                format!("{{\"$in\": [{}]}}", items.join(", "))
            }
            Op::IsTrue => "true".to_owned(),
            Op::IsFalse => "false".to_owned(),
            Op::IsEmpty => "null".to_owned(),
            Op::IsNotEmpty => "{\"$ne\": null}".to_owned(),
        };
        Ok(format!("{{{field}: {condition}}}"))
    }

    pub(super) fn condition(filter: &Filter) -> Result<String, String> {
        let parts = filter
            .rules
            .iter()
            .map(rule)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(match parts.len() {
            0 => String::new(),
            1 => parts.into_iter().next().unwrap_or_default(),
            _ => format!(
                "{{\"{}\": [{}]}}",
                if filter.any { "$or" } else { "$and" },
                parts.join(", ")
            ),
        })
    }
}

/// How a summary groups rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Bucket {
    /// Each distinct value.
    #[default]
    Value,
    /// The day of a date.
    Day,
    /// The month of a date.
    Month,
    /// The year of a date.
    Year,
}

impl Bucket {
    /// All buckets, as the summary lists them.
    pub const ALL: [Self; 4] = [Self::Value, Self::Day, Self::Month, Self::Year];

    /// The bucket as the summary lists it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Value => "each value",
            Self::Day => "day",
            Self::Month => "month",
            Self::Year => "year",
        }
    }
}

/// What a summary computes for each group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Measure {
    /// How many rows.
    #[default]
    Count,
    /// The sum of a column.
    Sum,
    /// The average of a column.
    Average,
    /// The smallest value of a column.
    Min,
    /// The largest value of a column.
    Max,
}

impl Measure {
    /// All measures, as the summary lists them.
    pub const ALL: [Self; 5] = [Self::Count, Self::Sum, Self::Average, Self::Min, Self::Max];

    /// The measure as the summary lists it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Count => "count of rows",
            Self::Sum => "sum of",
            Self::Average => "average of",
            Self::Min => "smallest",
            Self::Max => "largest",
        }
    }

    /// Whether the measure needs a column.
    pub fn needs_column(self) -> bool {
        self != Self::Count
    }
}

/// Groups and the measure to compute for each.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Summary {
    /// The column to group on; `None` for one total over every row.
    pub group: Option<String>,
    /// How the group column is bucketed.
    #[serde(default)]
    pub bucket: Bucket,
    /// What to compute.
    #[serde(default)]
    pub measure: Measure,
    /// The column the measure reads, when it reads one.
    #[serde(default)]
    pub of: Option<String>,
}

/// Groups a summary returns at most.
pub const SUMMARY_GROUPS: usize = 500;

/// Whether `kind` can summarize: Redis has no grouping, and Cassandra groups
/// only on its key columns.
pub fn summarizes(kind: DbKind) -> bool {
    !matches!(kind, DbKind::Redis | DbKind::Cassandra)
}

/// The statement that computes `summary` over the rows of `table` that
/// `condition` (from [`condition`]) keeps. It returns two columns, the
/// group and the measure, at most [`SUMMARY_GROUPS`] rows: buckets in
/// time order, values from the biggest measure down.
///
/// # Errors
///
/// A message when `kind` cannot summarize, or a measure has no column.
pub fn summary(
    kind: DbKind,
    table: &TableRef,
    condition: &str,
    summary: &Summary,
) -> Result<String, String> {
    if !summarizes(kind) {
        return Err(match kind {
            DbKind::Cassandra => {
                "Cassandra can only group on key columns; write a query for it.".into()
            }
            _ => "Redis keys cannot be summarized.".into(),
        });
    }
    if summary.measure.needs_column() && summary.of.is_none() {
        return Err("Choose the column to measure.".into());
    }
    if kind == DbKind::Mongodb {
        return Ok(mongo_summary(table, condition, summary));
    }
    let dialect = crate::driver::dialect(kind);
    let measure = match (summary.measure, &summary.of) {
        (Measure::Count, _) | (_, None) => "COUNT(*)".to_owned(),
        (measure, Some(column)) => {
            let column = dialect.quote_ident(column);
            match measure {
                Measure::Sum => format!("SUM({column})"),
                Measure::Average => format!("AVG({column})"),
                Measure::Min => format!("MIN({column})"),
                _ => format!("MAX({column})"),
            }
        }
    };
    let value_alias = dialect.quote_ident(measure_name(summary));
    let filter = condition.trim();
    let filter = if filter.is_empty() {
        String::new()
    } else {
        format!(" WHERE ({filter})")
    };
    let from = dialect.quote_table(table);
    let Some(group) = &summary.group else {
        return Ok(format!(
            "SELECT {measure} AS {value_alias} FROM {from}{filter}"
        ));
    };
    let column = dialect.quote_ident(group);
    let expression = bucket_sql(kind, &column, summary.bucket);
    let group_alias = dialect.quote_ident(group);
    let order = if summary.bucket == Bucket::Value {
        format!("{measure} DESC")
    } else {
        expression.clone()
    };
    let limit = SUMMARY_GROUPS;
    Ok(match kind {
        DbKind::SqlServer => format!(
            "SELECT TOP {limit} {expression} AS {group_alias}, {measure} AS {value_alias} \
             FROM {from}{filter} GROUP BY {expression} ORDER BY {order}"
        ),
        _ => format!(
            "SELECT {expression} AS {group_alias}, {measure} AS {value_alias} \
             FROM {from}{filter} GROUP BY {expression} ORDER BY {order} LIMIT {limit}"
        ),
    })
}

/// The measure column's name in the result.
fn measure_name(summary: &Summary) -> &'static str {
    match summary.measure {
        Measure::Count => "count",
        Measure::Sum => "sum",
        Measure::Average => "average",
        Measure::Min => "min",
        Measure::Max => "max",
    }
}

fn bucket_sql(kind: DbKind, column: &str, bucket: Bucket) -> String {
    let unit = match bucket {
        Bucket::Value => return column.to_owned(),
        Bucket::Day => "day",
        Bucket::Month => "month",
        Bucket::Year => "year",
    };
    match kind {
        DbKind::Mysql | DbKind::Mariadb => {
            let format = match bucket {
                Bucket::Day => "%Y-%m-%d",
                Bucket::Month => "%Y-%m",
                _ => "%Y",
            };
            format!("DATE_FORMAT({column}, '{format}')")
        }
        DbKind::Sqlite => {
            let format = match bucket {
                Bucket::Day => "%Y-%m-%d",
                Bucket::Month => "%Y-%m",
                _ => "%Y",
            };
            format!("strftime('{format}', {column})")
        }
        DbKind::SqlServer => {
            let format = match bucket {
                Bucket::Day => "yyyy-MM-dd",
                Bucket::Month => "yyyy-MM",
                _ => "yyyy",
            };
            format!("FORMAT({column}, '{format}')")
        }
        DbKind::Elasticsearch => format!("DATE_TRUNC('{unit}', {column})"),
        _ => format!("DATE_TRUNC('{unit}', {column})"),
    }
}

fn mongo_summary(table: &TableRef, condition: &str, summary: &Summary) -> String {
    let collection = crate::driver::dialect(DbKind::Mongodb).quote_table(table);
    let field = |name: &str| mongo_string(&format!("${name}"));
    let id = match &summary.group {
        None => "null".to_owned(),
        Some(group) => match summary.bucket {
            Bucket::Value => field(group),
            bucket => format!(
                "{{\"$dateTrunc\": {{\"date\": {}, \"unit\": \"{}\"}}}}",
                field(group),
                match bucket {
                    Bucket::Day => "day",
                    Bucket::Month => "month",
                    _ => "year",
                }
            ),
        },
    };
    let of = summary.of.as_deref().map(field).unwrap_or_default();
    let measure = match summary.measure {
        Measure::Count => "{\"$sum\": 1}".to_owned(),
        Measure::Sum => format!("{{\"$sum\": {of}}}"),
        Measure::Average => format!("{{\"$avg\": {of}}}"),
        Measure::Min => format!("{{\"$min\": {of}}}"),
        Measure::Max => format!("{{\"$max\": {of}}}"),
    };
    let name = measure_name(summary);
    let group_name = mongo_string(summary.group.as_deref().unwrap_or_default());
    let sort = if summary.bucket == Bucket::Value {
        format!("{{\"{name}\": -1}}")
    } else {
        "{\"_id\": 1}".to_owned()
    };
    let mut stages = Vec::new();
    if !condition.trim().is_empty() {
        stages.push(format!("{{\"$match\": {}}}", condition.trim()));
    }
    stages.push(format!(
        "{{\"$group\": {{\"_id\": {id}, \"{name}\": {measure}}}}}"
    ));
    stages.push(format!("{{\"$sort\": {sort}}}"));
    stages.push(format!("{{\"$limit\": {SUMMARY_GROUPS}}}"));
    stages.push(match summary.group {
        Some(_) => {
            format!("{{\"$project\": {{\"_id\": 0, {group_name}: \"$_id\", \"{name}\": 1}}}}")
        }
        None => format!("{{\"$project\": {{\"_id\": 0, \"{name}\": 1}}}}"),
    });
    format!("{collection}.aggregate([{}])", stages.join(", "))
}

fn mongo_string(text: &str) -> String {
    serde_json::Value::String(text.to_owned()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(kind: DbKind, rule: Rule) -> String {
        condition(
            kind,
            &Filter {
                rules: vec![rule],
                any: false,
            },
        )
        .unwrap()
    }

    #[test]
    fn class_should_follow_the_type_name() {
        assert_eq!(Class::of("integer"), Class::Number);
        assert_eq!(Class::of("numeric(10,2)"), Class::Number);
        assert_eq!(Class::of("timestamp with time zone"), Class::Date);
        assert_eq!(Class::of("datetime2"), Class::Date);
        assert_eq!(Class::of("tinyint(1)"), Class::Bool);
        assert_eq!(Class::of("boolean"), Class::Bool);
        assert_eq!(Class::of("jsonb"), Class::Nested);
        assert_eq!(Class::of("set<text>"), Class::Nested);
        assert_eq!(Class::of("text[]"), Class::Nested);
        assert_eq!(Class::of("varchar(20)"), Class::Text);
        assert_eq!(Class::of("interval"), Class::Text);
        assert_eq!(Class::of("long"), Class::Number);
    }

    #[test]
    fn text_rules_should_quote_and_escape() {
        assert_eq!(
            one(
                DbKind::Postgres,
                Rule::new("name", "text", Op::Contains, "50%_o'k")
            ),
            "CAST(\"name\" AS text) ILIKE '%50!%!_o''k%' ESCAPE '!'"
        );
        assert_eq!(
            one(
                DbKind::Mysql,
                Rule::new("name", "varchar(9)", Op::Is, "a\\b")
            ),
            "`name` = 'a\\\\b'"
        );
        assert_eq!(
            one(
                DbKind::SqlServer,
                Rule::new("name", "nvarchar", Op::StartsWith, "[x")
            ),
            "[name] LIKE N'![x%' ESCAPE '!'"
        );
    }

    #[test]
    fn number_rules_should_refuse_words() {
        let filter = Filter {
            rules: vec![Rule::new("id", "integer", Op::Greater, "ten")],
            any: false,
        };

        assert!(condition(DbKind::Postgres, &filter).is_err());
        let soon = Filter {
            rules: vec![Rule::new("at", "date", Op::Before, "soon")],
            any: false,
        };
        assert!(
            condition(DbKind::Postgres, &soon)
                .unwrap_err()
                .contains("needs a date")
        );
        assert_eq!(
            one(
                DbKind::Postgres,
                Rule::new("id", "integer", Op::OneOf, "1, 2,3")
            ),
            "\"id\" IN (1, 2, 3)"
        );
    }

    #[test]
    fn a_day_on_a_date_column_should_cover_the_whole_day() {
        assert_eq!(
            one(
                DbKind::Postgres,
                Rule::new("at", "timestamptz", Op::Is, "2024-02-29")
            ),
            "\"at\" >= '2024-02-29' AND \"at\" < '2024-03-01'"
        );
    }

    #[test]
    fn rules_should_join_with_and_or_or() {
        let mut filter = Filter {
            rules: vec![
                Rule::new("a", "int", Op::Is, "1"),
                Rule::new("b", "bool", Op::IsTrue, ""),
            ],
            any: false,
        };
        assert_eq!(
            condition(DbKind::Sqlite, &filter).unwrap(),
            "\"a\" = 1 AND \"b\" = 1"
        );
        filter.any = true;
        assert_eq!(
            condition(DbKind::Sqlite, &filter).unwrap(),
            "(\"a\" = 1) OR (\"b\" = 1)"
        );
        assert!(condition(DbKind::Cassandra, &filter).is_err());
    }

    #[test]
    fn mongo_rules_should_make_a_filter_document() {
        let filter = Filter {
            rules: vec![
                Rule::new("status", "string", Op::OneOf, "paid, shipped"),
                Rule::new("total", "double", Op::Between, "1"),
                Rule::new("name", "string", Op::StartsWith, "a.b"),
            ],
            any: true,
        };
        let mut filter = filter;
        filter.rules[1].value2 = "9".into();

        assert_eq!(
            condition(DbKind::Mongodb, &filter).unwrap(),
            "{\"$or\": [{\"status\": {\"$in\": [\"paid\", \"shipped\"]}}, \
             {\"total\": {\"$gte\": 1, \"$lte\": 9}}, \
             {\"name\": {\"$regex\": \"^a\\\\.b\", \"$options\": \"i\"}}]}"
        );
    }

    #[test]
    fn elasticsearch_should_search_any_field_and_cast_dates() {
        assert_eq!(
            one(
                DbKind::Elasticsearch,
                Rule::new(ANY_FIELD, "", Op::Matches, "lamp")
            ),
            "QUERY('lamp')"
        );
        assert_eq!(
            one(
                DbKind::Elasticsearch,
                Rule::new("at", "datetime", Op::After, "2024-01-01")
            ),
            "\"at\" > CAST('2024-01-01' AS DATETIME)"
        );
    }

    #[test]
    fn summaries_should_group_per_database() {
        let by_month = Summary {
            group: Some("placed_at".into()),
            bucket: Bucket::Month,
            measure: Measure::Sum,
            of: Some("total".into()),
        };
        let table = TableRef::new("shop", "orders");

        assert_eq!(
            summary(DbKind::Postgres, &table, "\"id\" > 1", &by_month).unwrap(),
            "SELECT DATE_TRUNC('month', \"placed_at\") AS \"placed_at\", SUM(\"total\") AS \"sum\" \
             FROM \"shop\".\"orders\" WHERE (\"id\" > 1) GROUP BY DATE_TRUNC('month', \"placed_at\") \
             ORDER BY DATE_TRUNC('month', \"placed_at\") LIMIT 500"
        );
        assert!(
            summary(DbKind::SqlServer, &table, "", &by_month)
                .unwrap()
                .starts_with("SELECT TOP 500 FORMAT([placed_at], 'yyyy-MM')")
        );
        assert_eq!(
            summary(
                DbKind::Mongodb,
                &TableRef::unqualified("orders"),
                "",
                &Summary {
                    group: Some("status".into()),
                    ..Summary::default()
                }
            )
            .unwrap(),
            "db.getCollection(\"orders\").aggregate([{\"$group\": {\"_id\": \"$status\", \"count\": {\"$sum\": 1}}}, \
             {\"$sort\": {\"count\": -1}}, {\"$limit\": 500}, \
             {\"$project\": {\"_id\": 0, \"status\": \"$_id\", \"count\": 1}}])"
        );
        assert!(summary(DbKind::Redis, &table, "", &Summary::default()).is_err());
    }
}
