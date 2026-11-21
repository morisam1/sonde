//! Plumbing the newer drivers share: running work on the driver runtime,
//! feeding pages to a [`QueryHandle`], cancelling, and turning documents into
//! rows.

use std::{collections::HashMap, sync::Arc, time::Instant};

use futures::{
    SinkExt as _,
    channel::{mpsc, oneshot},
};
use tokio::sync::watch;

use crate::{
    driver::{CHANNEL_PAGES, Canceller, PAGE_ROWS, QueryHandle, QuerySummary},
    error::{Error, Result},
    runtime,
    value::{ColumnMeta, ResultPage, Value},
};

/// Run `future` on the driver runtime and wait for it from any executor.
pub(crate) async fn bridge<T, F>(future: F) -> Result<T>
where
    F: Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    runtime::spawn(future).await.unwrap_or(Err(Error::Closed))
}

/// The start of a statement's result: nothing is known about its columns
/// until [`columns`](Self::columns) is called.
pub(crate) struct Ready {
    ready: oneshot::Sender<Result<Vec<ColumnMeta>>>,
    pages: mpsc::Sender<Result<ResultPage>>,
    summary: oneshot::Sender<QuerySummary>,
    started: Instant,
}

impl Ready {
    /// The statement failed before it produced a result.
    pub(crate) fn fail(self, error: Error) {
        let _ = self.ready.send(Err(error));
    }

    /// The result has these columns; rows follow through the returned feed.
    pub(crate) fn columns(self, columns: Vec<ColumnMeta>) -> Feed {
        let width = columns.len();
        let _ = self.ready.send(Ok(columns));
        Feed {
            pages: self.pages,
            summary: self.summary,
            started: self.started,
            rows: Vec::new(),
            returned: 0,
            width,
        }
    }

    /// A statement that returns no rows.
    pub(crate) async fn done(self, rows_affected: Option<u64>, notices: Vec<String>) {
        self.columns(Vec::new())
            .finish(rows_affected, notices)
            .await;
    }
}

/// Pages of rows on their way to a [`QueryHandle`].
pub(crate) struct Feed {
    pages: mpsc::Sender<Result<ResultPage>>,
    summary: oneshot::Sender<QuerySummary>,
    started: Instant,
    rows: Vec<Vec<Value>>,
    returned: u64,
    width: usize,
}

impl Feed {
    /// Queue a row, padded or cut to the result's width. Returns `false`
    /// once nobody is reading any more, when the work should stop.
    pub(crate) async fn push(&mut self, mut row: Vec<Value>) -> bool {
        row.resize(self.width, Value::Null);
        self.rows.push(row);
        self.returned += 1;
        if self.rows.len() >= PAGE_ROWS {
            return self.flush().await;
        }
        true
    }

    async fn flush(&mut self) -> bool {
        if self.rows.is_empty() {
            return !self.pages.is_closed();
        }
        let rows = std::mem::take(&mut self.rows);
        self.pages.send(Ok(ResultPage { rows })).await.is_ok()
    }

    /// End with an error, after whatever rows were already sent.
    pub(crate) async fn fail(mut self, error: Error) {
        if self.flush().await {
            let _ = self.pages.send(Err(error)).await;
        }
    }

    /// End successfully.
    pub(crate) async fn finish(mut self, rows_affected: Option<u64>, notices: Vec<String>) {
        if !self.flush().await {
            return;
        }
        self.pages.close_channel();
        let _ = self.summary.send(QuerySummary {
            rows_affected,
            rows_returned: self.returned,
            elapsed: self.started.elapsed(),
            notices,
        });
    }
}

/// Spawn `work` on the driver runtime and return the handle once it has
/// named its columns (or failed).
pub(crate) async fn start<F, Fut>(work: F) -> Result<QueryHandle>
where
    F: FnOnce(Ready) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (ready, ready_rx) = oneshot::channel();
    let (pages, pages_rx) = mpsc::channel(CHANNEL_PAGES);
    let (summary, summary_rx) = oneshot::channel();
    runtime::handle().spawn(work(Ready {
        ready,
        pages,
        summary,
        started: Instant::now(),
    }));
    let columns = ready_rx.await.map_err(|_| Error::Closed)??;
    Ok(QueryHandle {
        columns,
        pages: pages_rx,
        summary: summary_rx,
    })
}

/// A session's cancel button: every press ends whatever is running then.
#[derive(Clone)]
pub(crate) struct CancelSignal(Arc<watch::Sender<u64>>);

impl CancelSignal {
    pub(crate) fn new() -> Self {
        Self(Arc::new(watch::channel(0).0))
    }

    pub(crate) fn canceller(&self) -> Canceller {
        let sender = Arc::clone(&self.0);
        Canceller::new(move || sender.send_modify(|presses| *presses += 1))
    }

    /// A future that resolves at the next press after this call.
    pub(crate) fn pressed(&self) -> impl Future<Output = ()> + Send + 'static {
        let mut presses = self.0.subscribe();
        presses.mark_unchanged();
        async move {
            if presses.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }
}

/// The columns of a result built from documents: every top-level key in
/// the order first seen, with the type it had, or `mixed` when it varies.
#[derive(Debug, Default)]
pub(crate) struct Columns {
    names: Vec<String>,
    types: Vec<String>,
    index: HashMap<String, usize>,
}

impl Columns {
    pub(crate) fn add(&mut self, name: &str, type_name: &str) {
        match self.index.get(name) {
            Some(&ix) => {
                let known = &mut self.types[ix];
                if known == "null" {
                    *known = type_name.to_owned();
                } else if known != type_name && type_name != "null" {
                    "mixed".clone_into(known);
                }
            }
            None => {
                self.index.insert(name.to_owned(), self.names.len());
                self.names.push(name.to_owned());
                self.types.push(type_name.to_owned());
            }
        }
    }

    pub(crate) fn position(&self, name: &str) -> Option<usize> {
        self.index.get(name).copied()
    }

    pub(crate) fn len(&self) -> usize {
        self.names.len()
    }

    pub(crate) fn names(&self) -> &[String] {
        &self.names
    }

    pub(crate) fn types(&self) -> &[String] {
        &self.types
    }

    pub(crate) fn meta(&self) -> Vec<ColumnMeta> {
        self.names
            .iter()
            .zip(&self.types)
            .map(|(name, type_name)| meta(name, type_name))
            .collect()
    }
}

/// A result column with no known origin.
pub(crate) fn meta(name: &str, type_name: &str) -> ColumnMeta {
    ColumnMeta {
        name: Arc::from(name),
        type_name: Arc::from(type_name),
        origin: None,
    }
}

/// A JSON value as a cell. Objects and arrays stay JSON.
pub(crate) fn json_value(value: &serde_json::Value) -> Value {
    match value {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => n
            .as_i64()
            .map(Value::Int)
            .or_else(|| n.as_f64().map(Value::Float))
            .unwrap_or_else(|| Value::text(n.to_string())),
        serde_json::Value::String(s) => Value::text(s.as_str()),
        other => Value::Json(Arc::from(other.to_string())),
    }
}

/// The type name a JSON value is shown with.
pub(crate) fn json_type(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(n) if n.is_i64() || n.is_u64() => "long",
        serde_json::Value::Number(_) => "double",
        serde_json::Value::String(_) => "text",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// A list of JSON objects as a table: the union of their keys as columns.
/// Anything else in the list becomes a single `value` column.
pub(crate) fn json_table(items: &[serde_json::Value]) -> (Vec<ColumnMeta>, Vec<Vec<Value>>) {
    if !items.is_empty() && items.iter().all(serde_json::Value::is_object) {
        let mut columns = Columns::default();
        for item in items {
            for (key, value) in item.as_object().into_iter().flatten() {
                columns.add(key, json_type(value));
            }
        }
        let rows = items
            .iter()
            .map(|item| {
                let mut row = vec![Value::Null; columns.len()];
                for (key, value) in item.as_object().into_iter().flatten() {
                    if let Some(ix) = columns.position(key) {
                        row[ix] = json_value(value);
                    }
                }
                row
            })
            .collect();
        return (columns.meta(), rows);
    }
    let type_name = items.first().map_or("json", json_type);
    (
        vec![meta("value", type_name)],
        items.iter().map(|item| vec![json_value(item)]).collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_table_should_union_keys_in_first_seen_order() {
        let items: Vec<serde_json::Value> =
            serde_json::from_str(r#"[{"a":1,"b":"x"},{"c":true,"a":2.5}]"#).unwrap();

        let (columns, rows) = json_table(&items);

        let names: Vec<&str> = columns.iter().map(|c| &*c.name).collect();
        assert_eq!(names, ["a", "b", "c"]);
        assert_eq!(&*columns[0].type_name, "mixed");
        assert_eq!(rows[1], [Value::Float(2.5), Value::Null, Value::Bool(true)]);
    }

    #[test]
    fn json_table_should_put_scalars_in_one_column() {
        let items: Vec<serde_json::Value> = serde_json::from_str(r#"["a","b"]"#).unwrap();

        let (columns, rows) = json_table(&items);

        assert_eq!(&*columns[0].name, "value");
        assert_eq!(rows.len(), 2);
    }
}
