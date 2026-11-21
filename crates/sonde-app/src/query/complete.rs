//! The SQL editor's completion provider, fed by a per-connection cache of
//! the catalog and the columns of tables that have come up.

use std::{
    cell::RefCell,
    collections::HashMap,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use gpui_kit::component::{Rope, RopeExt as _, input::CompletionProvider};
use gpui_kit::*;
use lsp_types::{
    CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit,
    CompletionTriggerKind, TextEdit,
};
use sonde_core::{
    Catalog, DbKind, Session, TableRef,
    complete::{self, Candidate, CandidateKind},
    schema::ColumnInfo,
};

use super::QueryPanel;

/// How long a fetched catalog is trusted before it is fetched again.
const FRESH_FOR: Duration = Duration::from_secs(60);

/// What is known about one connection's schema.
#[derive(Default)]
struct Known {
    catalog: Option<(Catalog, Instant)>,
    columns: HashMap<TableRef, Vec<ColumnInfo>>,
}

/// The schema cache of every connection, shared by its tabs.
#[derive(Default)]
pub struct SchemaCache(HashMap<String, Rc<RefCell<Known>>>);

impl Global for SchemaCache {}

impl SchemaCache {
    fn entry(connection_id: &str, cx: &mut App) -> Rc<RefCell<Known>> {
        let cache = cx.default_global::<Self>();
        Rc::clone(cache.0.entry(connection_id.to_owned()).or_default())
    }

    /// Forget what is known about a connection, after its schema changed.
    pub fn forget(connection_id: &str, cx: &mut App) {
        if let Some(cache) = cx.try_global::<Self>()
            && cache.0.contains_key(connection_id)
        {
            cx.global_mut::<Self>().0.remove(connection_id);
        }
    }
}

/// Work out the completions at `cursor` in `text`, fetching the catalog and
/// any missing columns through `session` first. With no session (one is busy
/// running a statement), only what is cached is used.
pub(super) fn candidates(
    connection_id: &str,
    kind: DbKind,
    session: Option<Arc<dyn Session>>,
    text: String,
    cursor: usize,
    explicit: bool,
    cx: &mut App,
) -> Task<Vec<Candidate>> {
    let Some(analysis) = complete::analyze(&text, cursor, kind, explicit) else {
        return Task::ready(Vec::new());
    };
    let known = SchemaCache::entry(connection_id, cx);
    cx.spawn(async move |_| {
        let stale = known
            .borrow()
            .catalog
            .as_ref()
            .is_none_or(|(_, at)| at.elapsed() > FRESH_FOR);
        if stale
            && let Some(session) = &session
            && let Ok(catalog) = session.catalog().await
        {
            let mut known = known.borrow_mut();
            known.catalog = Some((catalog, Instant::now()));
            known.columns.clear();
        }
        let needed: Vec<TableRef> = {
            let known = known.borrow();
            let Some((catalog, _)) = &known.catalog else {
                return Vec::new();
            };
            analysis
                .tables_needed(catalog)
                .into_iter()
                .filter(|table| !known.columns.contains_key(table))
                .collect()
        };
        if let Some(session) = &session {
            for table in needed {
                if let Ok(detail) = session.table_detail(&table).await {
                    known.borrow_mut().columns.insert(table, detail.columns);
                }
            }
        }
        let known = known.borrow();
        let Some((catalog, _)) = &known.catalog else {
            return Vec::new();
        };
        analysis.candidates(catalog, kind, |table| {
            known.columns.get(table).map(Vec::as_slice)
        })
    })
}

/// Plugs [`candidates`] into the editor.
pub(super) struct Provider {
    pub(super) panel: WeakEntity<QueryPanel>,
}

impl CompletionProvider for Provider {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        trigger: CompletionContext,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<CompletionResponse>> {
        let Some(panel) = self.panel.upgrade() else {
            return Task::ready(Ok(CompletionResponse::Array(Vec::new())));
        };
        let explicit = trigger.trigger_kind == CompletionTriggerKind::INVOKED;
        let (id, kind, session) = {
            let panel = panel.read(cx);
            (
                panel.target.id.clone(),
                panel.target.config.kind(),
                panel.idle_session(),
            )
        };
        let source = text.to_string();
        let rope = text.clone();
        let task = candidates(&id, kind, session, source, offset, explicit, cx);
        let replace = complete::analyze(&rope.to_string(), offset, kind, explicit)
            .map(|analysis| analysis.replace);
        cx.spawn(async move |_| {
            let found = task.await;
            let Some(replace) = replace else {
                return Ok(CompletionResponse::Array(Vec::new()));
            };
            let range = lsp_types::Range {
                start: rope.offset_to_position(replace.start),
                end: rope.offset_to_position(replace.end),
            };
            let items = found
                .into_iter()
                .map(|candidate| item(candidate, range))
                .collect();
            Ok(CompletionResponse::Array(items))
        })
    }

    fn is_completion_trigger(&self, _: usize, new_text: &str, _: &mut App) -> bool {
        new_text
            .chars()
            .last()
            .is_some_and(|c| c == '.' || c == '_' || c == '$' || c.is_alphanumeric())
    }
}

fn item(candidate: Candidate, range: lsp_types::Range) -> CompletionItem {
    CompletionItem {
        kind: Some(match candidate.kind {
            CandidateKind::Column => CompletionItemKind::FIELD,
            CandidateKind::Table => CompletionItemKind::CLASS,
            CandidateKind::View => CompletionItemKind::INTERFACE,
            CandidateKind::Schema => CompletionItemKind::MODULE,
            CandidateKind::Keyword => CompletionItemKind::KEYWORD,
        }),
        detail: candidate.detail,
        text_edit: Some(CompletionTextEdit::Edit(TextEdit {
            range,
            new_text: candidate.insert,
        })),
        label: candidate.label,
        ..CompletionItem::default()
    }
}
