mod handlers;
mod query;

use crate::{
    alt,
    compositor::{self, Component, Compositor, Context, Event, EventResult},
    ctrl, key, shift,
    ui::{
        self,
        document::{render_document, LinePos, TextRenderer},
        picker::query::PickerQuery,
        text_decorations::DecorationManager,
        EditorView,
    },
};
use futures_util::future::BoxFuture;
use helix_event::AsyncHook;
use nucleo::pattern::{CaseMatching, Normalization};
use nucleo::{Config, Nucleo};
use thiserror::Error;
use tokio::sync::mpsc::Sender;
use tui::{
    buffer::Buffer as Surface,
    layout::Constraint,
    text::{Span, Spans},
    widgets::{Block, BorderType, Cell, Row, Table},
};

use tui::widgets::Widget;

use std::{
    borrow::Cow,
    collections::HashMap,
    io::Read,
    path::Path,
    sync::{
        atomic::{self, AtomicUsize},
        Arc,
    },
    time::{Duration, Instant},
};

use crate::ui::{Prompt, PromptEvent};
use helix_core::{
    char_idx_at_visual_offset, fuzzy::MATCHER, movement::Direction,
    text_annotations::TextAnnotations, unicode::segmentation::UnicodeSegmentation, Position,
};
use helix_view::{
    editor::Action,
    graphics::{CursorKind, Margin, Modifier, Rect},
    input::{KeyEvent, MouseButton, MouseEvent, MouseEventKind},
    keyboard::{KeyCode, KeyModifiers},
    theme::Style,
    view::ViewPosition,
    Document, DocumentId, Editor,
};

use self::handlers::{DynamicQueryChange, DynamicQueryHandler, PreviewHighlightHandler};

pub const ID: &str = "picker";

/// Two clicks on the same row closer than this are a double click: the terminal
/// reports each press on its own, so the picker has to tell them apart itself.
const DOUBLE_CLICK: Duration = Duration::from_millis(500);

/// The wheel's scroll of the preview, and the row it was given on.
#[derive(Default)]
struct PreviewScroll {
    row: u32,
    lines: isize,
}

pub const MIN_AREA_WIDTH_FOR_PREVIEW: u16 = 72;
/// Biggest file size to preview in bytes
pub const MAX_FILE_SIZE_FOR_PREVIEW: u64 = 10 * 1024 * 1024;

#[derive(PartialEq, Eq, Hash)]
pub enum PathOrId<'a> {
    Id(DocumentId),
    Path(&'a Path),
}

impl<'a> From<&'a Path> for PathOrId<'a> {
    fn from(path: &'a Path) -> Self {
        Self::Path(path)
    }
}

impl From<DocumentId> for PathOrId<'_> {
    fn from(v: DocumentId) -> Self {
        Self::Id(v)
    }
}

type FileCallback<T> = Box<dyn for<'a> Fn(&'a Editor, &'a T) -> Option<FileLocation<'a>>>;

/// File path and range of lines (used to align and highlight lines)
pub type FileLocation<'a> = (PathOrId<'a>, Option<(usize, usize)>);

pub enum CachedPreview {
    Document(Box<Document>),
    Directory(Vec<(String, bool)>),
    Binary,
    LargeFile,
    NotFound,
}

// We don't store this enum in the cache so as to avoid lifetime constraints
// from borrowing a document already opened in the editor.
pub enum Preview<'picker, 'editor> {
    Cached(&'picker CachedPreview),
    EditorDocument(&'editor Document),
}

impl Preview<'_, '_> {
    fn document(&self) -> Option<&Document> {
        match self {
            Preview::EditorDocument(doc) => Some(doc),
            Preview::Cached(CachedPreview::Document(doc)) => Some(doc),
            _ => None,
        }
    }

    fn dir_content(&self) -> Option<&Vec<(String, bool)>> {
        match self {
            Preview::Cached(CachedPreview::Directory(dir_content)) => Some(dir_content),
            _ => None,
        }
    }

    /// Alternate text to show for the preview.
    fn placeholder(&self) -> &str {
        match *self {
            Self::EditorDocument(_) => "<Invalid file location>",
            Self::Cached(preview) => match preview {
                CachedPreview::Document(_) => "<Invalid file location>",
                CachedPreview::Directory(_) => "<Invalid directory location>",
                CachedPreview::Binary => "<Binary file>",
                CachedPreview::LargeFile => "<File too large to preview>",
                CachedPreview::NotFound => "<File not found>",
            },
        }
    }
}

fn inject_nucleo_item<T, D>(
    injector: &nucleo::Injector<T>,
    columns: &[Column<T, D>],
    item: T,
    editor_data: &D,
) {
    injector.push(item, |item, dst| {
        for (column, text) in columns.iter().filter(|column| column.filter).zip(dst) {
            *text = column.format_text(item, editor_data).into()
        }
    });
}

pub struct Injector<T, D> {
    dst: nucleo::Injector<T>,
    columns: Arc<[Column<T, D>]>,
    editor_data: Arc<D>,
    version: usize,
    picker_version: Arc<AtomicUsize>,
    /// A marker that requests a redraw when the injector drops.
    /// This marker causes the "running" indicator to disappear when a background job
    /// providing items is finished and drops. This could be wrapped in an [Arc] to ensure
    /// that the redraw is only requested when all Injectors drop for a Picker (which removes
    /// the "running" indicator) but the redraw handle is debounced so this is unnecessary.
    _redraw: helix_event::RequestRedrawOnDrop,
}

impl<I, D> Clone for Injector<I, D> {
    fn clone(&self) -> Self {
        Injector {
            dst: self.dst.clone(),
            columns: self.columns.clone(),
            editor_data: self.editor_data.clone(),
            version: self.version,
            picker_version: self.picker_version.clone(),
            _redraw: helix_event::RequestRedrawOnDrop,
        }
    }
}

#[derive(Error, Debug)]
#[error("picker has been shut down")]
pub struct InjectorShutdown;

impl<T, D> Injector<T, D> {
    pub fn push(&self, item: T) -> Result<(), InjectorShutdown> {
        if self.version != self.picker_version.load(atomic::Ordering::Relaxed) {
            return Err(InjectorShutdown);
        }

        inject_nucleo_item(&self.dst, &self.columns, item, &self.editor_data);
        Ok(())
    }
}

type ColumnFormatFn<T, D> = for<'a> fn(&'a T, &'a D) -> Cell<'a>;

pub struct Column<T, D> {
    name: Arc<str>,
    /// Which end of the cell to cut when it does not fit. A path keeps its tail,
    /// a line of text keeps its head.
    truncate_start: bool,
    format: ColumnFormatFn<T, D>,
    /// Whether the column should be passed to nucleo for matching and filtering.
    /// `DynamicPicker` uses this so that the dynamic column (for example regex in
    /// global search) is not used for filtering twice.
    filter: bool,
    hidden: bool,
    /// Whether each word typed must appear whole in the cell, in order of letters and
    /// together, rather than letter by letter anywhere: a line of prose read by fuzzy
    /// matching finds nearly everything.
    words: bool,
    /// The values this column takes, when they are few and known: a word typed for the
    /// primary column that is one of them narrows this column instead.
    keywords: &'static [&'static str],
}

impl<T, D> Column<T, D> {
    pub fn new(name: impl Into<Arc<str>>, format: ColumnFormatFn<T, D>) -> Self {
        Self {
            name: name.into(),
            truncate_start: true,
            format,
            filter: true,
            hidden: false,
            words: false,
            keywords: &[],
        }
    }

    /// A column which does not display any contents
    pub fn hidden(name: impl Into<Arc<str>>) -> Self {
        let format = |_: &T, _: &D| unreachable!();

        Self {
            name: name.into(),
            truncate_start: true,
            format,
            filter: false,
            hidden: true,
            words: false,
            keywords: &[],
        }
    }

    /// A column that is searched but not shown: what the others say, put together, so
    /// plain text typed in the query finds a row by any of them.
    pub fn searched_only(name: impl Into<Arc<str>>, format: ColumnFormatFn<T, D>) -> Self {
        Self {
            name: name.into(),
            truncate_start: true,
            format,
            filter: true,
            hidden: true,
            words: true,
            keywords: &[],
        }
    }

    pub fn without_filtering(mut self) -> Self {
        self.filter = false;
        self
    }

    /// Names the values this column takes, so typing one of them whole narrows the
    /// column without `%name` before it.
    pub fn with_keywords(mut self, keywords: &'static [&'static str]) -> Self {
        self.keywords = keywords;
        self
    }

    /// Cut this column's tail rather than its head when it does not fit.
    pub fn keeping_start(mut self) -> Self {
        self.truncate_start = false;
        self
    }

    fn format<'a>(&self, item: &'a T, data: &'a D) -> Cell<'a> {
        (self.format)(item, data)
    }

    fn format_text<'a>(&self, item: &'a T, data: &'a D) -> Cow<'a, str> {
        let text: String = self.format(item, data).content.into();
        text.into()
    }
}

/// Returns a new list of options to replace the contents of the picker
/// when called with the current picker query,
type DynQueryCallback<T, D> =
    fn(&PanelInput, &mut Editor, Arc<D>, &Injector<T, D>) -> BoxFuture<'static, anyhow::Result<()>>;

/// A switch drawn at the right end of the picker's query line. `Tab` walks onto it
/// and `Space` or a click flips it; `Alt` + `key` flips it from anywhere.
pub struct PanelToggle {
    name: &'static str,
    label: &'static str,
    key: char,
    on: bool,
    /// Said on the query line while the switch is off and a box it shows holds text.
    warning: Option<&'static str>,
}

impl PanelToggle {
    pub fn new(name: &'static str, label: &'static str, key: char, on: bool) -> Self {
        Self {
            name,
            label,
            key,
            on,
            warning: None,
        }
    }

    /// For a switch that hides boxes which still apply while hidden: what the query
    /// line says so nobody is narrowed by a filter they cannot see.
    pub fn warning_when_off(mut self, warning: &'static str) -> Self {
        self.warning = Some(warning);
        self
    }
}

/// A text box on its own line under the query line, reaching the dynamic query as a
/// field named `name`. An empty box asks for nothing.
pub struct PanelField {
    name: &'static str,
    label: &'static str,
    prompt: Prompt,
    /// The switch that shows the box; without one it is always there. A hidden box
    /// keeps its text and still reaches the query: what it means hidden is the
    /// owner's to decide.
    shown_by: Option<&'static str>,
    /// A button at the right end of the line that runs the panel action.
    button: Option<&'static str>,
}

impl PanelField {
    pub fn new(name: &'static str, label: &'static str, placeholder: &'static str) -> Self {
        let prompt = Prompt::new(
            "".into(),
            None,
            ui::completers::none,
            |_editor: &mut Context, _pattern: &str, _event: PromptEvent| {},
        )
        .with_placeholder(placeholder);

        Self {
            name,
            label,
            prompt,
            shown_by: None,
            button: None,
        }
    }

    pub fn with_value(mut self, value: String, editor: &Editor) -> Self {
        self.prompt.set_line(value, editor);
        self
    }

    pub fn shown_by(mut self, toggle: &'static str) -> Self {
        self.shown_by = Some(toggle);
        self
    }

    pub fn with_button(mut self, label: &'static str) -> Self {
        self.button = Some(label);
        self
    }
}

/// Where the focus ring is. Tab walks it in this order: the query line, the boxes
/// under it, then the switches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Query,
    Field(usize),
    Toggle(usize),
}

type FieldChangeFn = Box<dyn Fn(&mut Editor, &PanelInput)>;

/// Every input of a picker at one moment: the query, the `%field` prefixes typed
/// beside it, the boxes under it and the switches. This is what a dynamic query is
/// re-run against, so flipping a switch refreshes the results the same way typing
/// does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelInput {
    query: Arc<str>,
    fields: Vec<(Arc<str>, Arc<str>)>,
    toggles: Vec<(&'static str, bool)>,
}

impl Default for PanelInput {
    fn default() -> Self {
        Self {
            query: "".into(),
            fields: Vec::new(),
            toggles: Vec::new(),
        }
    }
}

impl PanelInput {
    pub fn query(&self) -> &str {
        &self.query
    }

    /// The text of the named `%field`, empty when nothing was typed for it.
    pub fn field(&self, name: &str) -> &str {
        self.fields
            .iter()
            .find(|(field, _)| &**field == name)
            .map(|(_, value)| &**value)
            .unwrap_or_default()
    }

    /// Whether the named switch is on, false when the picker declares no such switch.
    pub fn toggle(&self, name: &str) -> bool {
        self.toggles
            .iter()
            .find(|(toggle, _)| *toggle == name)
            .is_some_and(|(_, on)| *on)
    }
}

/// Hands a picker's query to a wider search, in place of the picker.
type WidenFn = dyn Fn(&mut Compositor, &mut Context, String);

pub struct Picker<T: 'static + Send + Sync, D: 'static> {
    columns: Arc<[Column<T, D>]>,
    primary_column: usize,
    editor_data: Arc<D>,
    version: Arc<AtomicUsize>,
    matcher: Nucleo<T>,

    /// Current height of the completions box
    completion_height: u16,

    cursor: u32,
    prompt: Prompt,
    query: PickerQuery,

    /// Switches drawn at the right end of the query line. Empty for an ordinary
    /// picker, which then renders exactly as it always did.
    panel_toggles: Vec<PanelToggle>,
    /// Text boxes drawn one per line under the query line.
    panel_fields: Vec<PanelField>,
    /// Told whenever a box's text changes, so the owner can keep it.
    on_field_change: Option<FieldChangeFn>,
    focus: Focus,
    /// Where each switch was last drawn, so the cursor can sit on the focused one
    /// and a click can find it. Filled during render, which the compositor always
    /// runs before asking for the cursor.
    toggle_areas: Vec<Rect>,
    /// Where the query line and each box on screen were last drawn, for the cursor
    /// and for a click that lands on one of them. A box is known by its index.
    query_area: Rect,
    field_areas: Vec<(usize, Rect)>,
    /// Where a box's button was last drawn.
    button_area: Option<Rect>,
    /// Where the result rows were last drawn and which match the first of them is,
    /// so a click can be turned back into the row it landed on.
    rows_area: Rect,
    rows_offset: u32,
    /// The row last clicked and when, so a second click on it opens it.
    last_click: Option<(u32, Instant)>,
    /// Where the preview was last drawn, empty while there is none, so the wheel over it
    /// scrolls the preview and not the list.
    preview_area: Rect,
    /// How far the wheel moved the preview from where the row put it. It is the row's:
    /// another row starts where its own match is.
    preview_scroll: PreviewScroll,
    /// What `Alt-a` and a box's button do with the panel's inputs and the results
    /// on screen. The search panel replaces every match with it.
    panel_action: Option<PanelCallback<T>>,
    /// A line of help drawn on the bottom border, for what the picker cannot show
    /// on its own: the `%field` prefixes it hides, what its switches mean.
    hint: &'static [&'static str],
    /// What Ctrl-f does with the query, for a picker that has somewhere wider to look.
    widen: Option<Box<WidenFn>>,
    title: Option<String>,

    /// Whether to show the preview panel (default true)
    show_preview: bool,
    /// Constraints for tabular formatting
    widths: Vec<Constraint>,

    callback_fn: PickerCallback<T>,
    default_action: Action,

    pub truncate_start: bool,
    /// Caches paths to documents
    preview_cache: HashMap<Arc<Path>, CachedPreview>,
    read_buffer: Vec<u8>,
    /// Given an item in the picker, return the file path and line number to display.
    file_fn: Option<FileCallback<T>>,
    /// An event handler for syntax highlighting the currently previewed file.
    preview_highlight_handler: Sender<Arc<Path>>,
    dynamic_query_handler: Option<Sender<DynamicQueryChange>>,
}

impl<T: 'static + Send + Sync, D: 'static + Send + Sync> Picker<T, D> {
    pub fn stream(
        columns: impl IntoIterator<Item = Column<T, D>>,
        editor_data: D,
    ) -> (Nucleo<T>, Injector<T, D>) {
        let columns: Arc<[_]> = columns.into_iter().collect();
        let matcher_columns = columns.iter().filter(|col| col.filter).count() as u32;
        assert!(matcher_columns > 0);
        let matcher = Nucleo::new(
            Config::DEFAULT,
            Arc::new(helix_event::request_redraw),
            None,
            matcher_columns,
        );
        let streamer = Injector {
            dst: matcher.injector(),
            columns,
            editor_data: Arc::new(editor_data),
            version: 0,
            picker_version: Arc::new(AtomicUsize::new(0)),
            _redraw: helix_event::RequestRedrawOnDrop,
        };
        (matcher, streamer)
    }

    pub fn new<C, O, F>(
        columns: C,
        primary_column: usize,
        options: O,
        editor_data: D,
        callback_fn: F,
    ) -> Self
    where
        C: IntoIterator<Item = Column<T, D>>,
        O: IntoIterator<Item = T>,
        F: Fn(&mut Context, &T, Action) + 'static,
    {
        let columns: Arc<[_]> = columns.into_iter().collect();
        let matcher_columns = columns
            .iter()
            .filter(|col: &&Column<T, D>| col.filter)
            .count() as u32;
        assert!(matcher_columns > 0);
        let matcher = Nucleo::new(
            Config::DEFAULT,
            Arc::new(helix_event::request_redraw),
            None,
            matcher_columns,
        );
        let injector = matcher.injector();
        for item in options {
            inject_nucleo_item(&injector, &columns, item, &editor_data);
        }
        Self::with(
            matcher,
            columns,
            primary_column,
            Arc::new(editor_data),
            Arc::new(AtomicUsize::new(0)),
            callback_fn,
        )
    }

    pub fn with_stream(
        matcher: Nucleo<T>,
        primary_column: usize,
        injector: Injector<T, D>,
        callback_fn: impl Fn(&mut Context, &T, Action) + 'static,
    ) -> Self {
        Self::with(
            matcher,
            injector.columns,
            primary_column,
            injector.editor_data,
            injector.picker_version,
            callback_fn,
        )
    }

    fn with(
        matcher: Nucleo<T>,
        columns: Arc<[Column<T, D>]>,
        default_column: usize,
        editor_data: Arc<D>,
        version: Arc<AtomicUsize>,
        callback_fn: impl Fn(&mut Context, &T, Action) + 'static,
    ) -> Self {
        assert!(!columns.is_empty());

        let prompt = Prompt::new(
            "".into(),
            None,
            ui::completers::none,
            |_editor: &mut Context, _pattern: &str, _event: PromptEvent| {},
        );

        // A hidden column takes no room.
        let widths = columns
            .iter()
            .map(|column| match column.hidden {
                true => Constraint::Length(0),
                false => Constraint::Length(column.name.chars().count() as u16),
            })
            .collect();

        let query = PickerQuery::new(columns.iter().map(|col| &col.name).cloned(), default_column)
            .with_keywords(
                columns
                    .iter()
                    .filter(|column| !column.keywords.is_empty())
                    .map(|column| (column.name.clone(), column.keywords)),
            );

        Self {
            columns,
            primary_column: default_column,
            matcher,
            editor_data,
            version,
            cursor: 0,
            prompt,
            query,
            panel_toggles: Vec::new(),
            panel_fields: Vec::new(),
            on_field_change: None,
            focus: Focus::Query,
            toggle_areas: Vec::new(),
            query_area: Rect::default(),
            field_areas: Vec::new(),
            button_area: None,
            rows_area: Rect::default(),
            rows_offset: 0,
            last_click: None,
            preview_area: Rect::default(),
            preview_scroll: PreviewScroll::default(),
            panel_action: None,
            hint: &[],
            widen: None,
            title: None,
            truncate_start: true,
            show_preview: true,
            callback_fn: Box::new(callback_fn),
            default_action: Action::Replace,
            completion_height: 0,
            widths,
            preview_cache: HashMap::new(),
            read_buffer: Vec::with_capacity(1024),
            file_fn: None,
            preview_highlight_handler: PreviewHighlightHandler::<T, D>::default().spawn(),
            dynamic_query_handler: None,
        }
    }

    pub fn injector(&self) -> Injector<T, D> {
        Injector {
            dst: self.matcher.injector(),
            columns: self.columns.clone(),
            editor_data: self.editor_data.clone(),
            version: self.version.load(atomic::Ordering::Relaxed),
            picker_version: self.version.clone(),
            _redraw: helix_event::RequestRedrawOnDrop,
        }
    }

    pub fn truncate_start(mut self, truncate_start: bool) -> Self {
        self.truncate_start = truncate_start;
        self
    }

    pub fn with_preview(
        mut self,
        preview_fn: impl for<'a> Fn(&'a Editor, &'a T) -> Option<FileLocation<'a>> + 'static,
    ) -> Self {
        self.file_fn = Some(Box::new(preview_fn));
        // assumption: if we have a preview we are matching paths... If this is ever
        // not true this could be a separate builder function
        self.matcher.update_config(Config::DEFAULT.match_paths());
        self
    }

    pub fn with_history_register(mut self, history_register: Option<char>) -> Self {
        self.prompt.with_history_register(history_register);
        self
    }

    pub fn with_initial_cursor(mut self, cursor: u32) -> Self {
        self.cursor = cursor;
        self
    }

    /// Switches drawn at the right end of the query line, flipped with `Alt` + the
    /// switch's own key. Flipping one refreshes a dynamic query.
    pub fn with_toggles(mut self, toggles: Vec<PanelToggle>) -> Self {
        self.panel_toggles = toggles;
        self
    }

    /// Text boxes under the query line, one per line. `on_change` hears every edit
    /// to any of them.
    pub fn with_fields(
        mut self,
        fields: Vec<PanelField>,
        on_change: impl Fn(&mut Editor, &PanelInput) + 'static,
    ) -> Self {
        self.panel_fields = fields;
        self.on_field_change = Some(Box::new(on_change));
        self
    }

    /// What `Alt-a` runs: the panel's inputs and every result currently on screen.
    pub fn with_panel_action(
        mut self,
        action: impl Fn(&mut Context, &PanelInput, &[&T]) + 'static,
    ) -> Self {
        self.panel_action = Some(Box::new(action));
        self
    }

    /// What the picker is, drawn on its top border.
    pub fn with_title(mut self, title: String) -> Self {
        self.title = Some(title);
        self
    }

    /// Ctrl-f (Cmd-f) closes the picker and hands its query to `widen`, when `enabled`.
    pub fn with_widen(
        mut self,
        enabled: bool,
        widen: impl Fn(&mut Compositor, &mut Context, String) + 'static,
    ) -> Self {
        if enabled {
            self.widen = Some(Box::new(widen));
        }
        self
    }

    /// Starts with `query` typed in.
    pub fn with_query(mut self, query: String, editor: &Editor) -> Self {
        if !query.is_empty() {
            self.prompt.set_line(query, editor);
            self.handle_prompt_change(true);
        }
        self
    }

    pub fn with_hint(mut self, hint: &'static [&'static str]) -> Self {
        self.hint = hint;
        self
    }

    pub fn with_dynamic_query(
        mut self,
        callback: DynQueryCallback<T, D>,
        debounce_ms: Option<u64>,
    ) -> Self {
        let handler = DynamicQueryHandler::new(callback, debounce_ms).spawn();
        let event = DynamicQueryChange {
            input: self.panel_input(),
            // Treat the initial query as a paste.
            is_paste: true,
            rerun: false,
        };
        helix_event::send_blocking(&handler, event);
        self.dynamic_query_handler = Some(handler);
        self
    }

    pub fn with_default_action(mut self, action: Action) -> Self {
        self.default_action = action;
        self
    }

    /// Move the cursor by a number of lines, either down (`Forward`) or up (`Backward`)
    pub fn move_by(&mut self, amount: u32, direction: Direction) {
        let len = self.matcher.snapshot().matched_item_count();

        if len == 0 {
            // No results, can't move.
            return;
        }

        match direction {
            Direction::Forward => {
                self.cursor = self.cursor.saturating_add(amount) % len;
            }
            Direction::Backward => {
                self.cursor = self.cursor.saturating_add(len).saturating_sub(amount) % len;
            }
        }
    }

    /// Move the cursor down by exactly one page. After the last page comes the first page.
    pub fn page_up(&mut self) {
        self.move_by(self.completion_height as u32, Direction::Backward);
    }

    /// Move the cursor up by exactly one page. After the first page comes the last page.
    pub fn page_down(&mut self) {
        self.move_by(self.completion_height as u32, Direction::Forward);
    }

    /// Move the cursor to the first entry
    pub fn to_start(&mut self) {
        self.cursor = 0;
    }

    /// Move the cursor to the last entry
    pub fn to_end(&mut self) {
        self.cursor = self
            .matcher
            .snapshot()
            .matched_item_count()
            .saturating_sub(1);
    }

    pub fn selection(&self) -> Option<&T> {
        self.matcher
            .snapshot()
            .get_matched_item(self.cursor)
            .map(|item| item.data)
    }

    fn primary_query(&self) -> Arc<str> {
        self.query
            .get(&self.columns[self.primary_column].name)
            .cloned()
            .unwrap_or_else(|| "".into())
    }

    /// Whether a box is on screen: always, unless its switch is off.
    fn field_shown(&self, field: &PanelField) -> bool {
        let Some(name) = field.shown_by else {
            return true;
        };

        self.panel_toggles
            .iter()
            .any(|toggle| toggle.name == name && toggle.on)
    }

    /// The indices of the boxes on screen, top to bottom.
    fn shown_fields(&self) -> Vec<usize> {
        (0..self.panel_fields.len())
            .filter(|index| self.field_shown(&self.panel_fields[*index]))
            .collect()
    }

    fn panel_input(&self) -> PanelInput {
        let boxes = self.panel_fields.iter().map(|field| {
            let value: Arc<str> = field.prompt.line().as_str().into();
            (Arc::from(field.name), value)
        });
        let fields = self
            .columns
            .iter()
            .filter(|column| !column.filter)
            .filter_map(|column| {
                let value = self.query.get(&column.name)?;
                Some((column.name.clone(), value.clone()))
            })
            .chain(boxes)
            .collect();
        let toggles = self
            .panel_toggles
            .iter()
            .map(|toggle| (toggle.name, toggle.on))
            .collect();

        PanelInput {
            query: self.primary_query(),
            fields,
            toggles,
        }
    }

    /// Moves the focus ring: the query line, each box on screen, each switch, then
    /// back.
    fn focus_by(&mut self, direction: Direction) {
        let fields = self.shown_fields().into_iter().map(Focus::Field);
        let toggles = (0..self.panel_toggles.len()).map(Focus::Toggle);
        let stops: Vec<Focus> = std::iter::once(Focus::Query)
            .chain(fields)
            .chain(toggles)
            .collect();

        let at = stops
            .iter()
            .position(|stop| *stop == self.focus)
            .unwrap_or_default();
        let next = match direction {
            Direction::Forward => at + 1,
            Direction::Backward => at + stops.len() - 1,
        } % stops.len();

        self.focus = stops[next];
    }

    fn flip_toggle(&mut self, index: usize) {
        self.panel_toggles[index].on = !self.panel_toggles[index].on;

        // A box that has just been hidden can no longer hold the focus.
        if let Focus::Field(field) = self.focus {
            if !self.field_shown(&self.panel_fields[field]) {
                self.focus = Focus::Query;
            }
        }

        // A switch that only shows boxes changes no input the search reads.
        let name = self.panel_toggles[index].name;
        let shows_boxes = self
            .panel_fields
            .iter()
            .any(|field| field.shown_by == Some(name));
        if !shows_boxes {
            self.refresh_dynamic_query(true);
        }
    }

    /// Runs the panel action over every result on screen, as `Alt-a` and a box's
    /// button do.
    fn run_panel_action(&mut self, ctx: &mut Context) {
        let Some(action) = self.panel_action.as_ref() else {
            return;
        };

        let input = self.panel_input();
        let snapshot = self.matcher.snapshot();
        let results: Vec<&T> = (0..snapshot.matched_item_count())
            .filter_map(|index| snapshot.get_matched_item(index))
            .map(|item| item.data)
            .collect();

        action(ctx, &input, &results);

        // The action has changed the files under the results, so what is on screen
        // is now a list of matches that are no longer there. The inputs have not
        // changed, so the search must be told to run anyway.
        self.send_dynamic_query(true, true);
    }

    /// Hands an edit to the line that has the focus. Typing while a switch has it
    /// goes back to the query line, which is where the typing was meant for.
    fn input_handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        let index = match self.focus {
            Focus::Field(index) => index,
            Focus::Query | Focus::Toggle(_) => {
                self.focus = Focus::Query;
                return self.prompt_handle_event(event, cx);
            }
        };

        let before = self.panel_fields[index].prompt.line().clone();
        self.panel_fields[index].prompt.handle_event(event, cx);
        if *self.panel_fields[index].prompt.line() == before {
            return EventResult::Consumed(None);
        }

        self.cursor = 0;
        self.refresh_dynamic_query(matches!(event, Event::Paste(_)));

        if let Some(on_change) = &self.on_field_change {
            let input = self.panel_input();
            on_change(cx.editor, &input);
        }

        EventResult::Consumed(None)
    }

    fn header_height(&self) -> u16 {
        if self.columns.len() > 1 {
            1
        } else {
            0
        }
    }

    pub fn toggle_preview(&mut self) {
        self.show_preview = !self.show_preview;
    }

    /// Opens the selected match, as Enter does, and closes the picker.
    fn accept(&mut self, ctx: &mut Context) -> EventResult {
        if let Some(option) = self.selection() {
            (self.callback_fn)(ctx, option, self.default_action);
        }
        if let Some(history_register) = self.prompt.history_register() {
            if let Err(err) = ctx
                .editor
                .registers
                .push(history_register, self.primary_query().to_string())
            {
                ctx.editor.set_error(err.to_string());
            }
        }
        self.close()
    }

    fn close(&mut self) -> EventResult {
        // if the picker is very large don't store it as last_picker to avoid
        // excessive memory consumption
        let callback: compositor::Callback = if self.matcher.snapshot().item_count() > 1_000_000 {
            Box::new(|compositor: &mut Compositor, _ctx| {
                // remove the layer
                compositor.pop();
            })
        } else {
            // stop streaming in new items in the background, really we should
            // be restarting the stream somehow once the picker gets
            // reopened instead (like for an FS crawl) that would also remove the
            // need for the special case above but that is pretty tricky
            self.version.fetch_add(1, atomic::Ordering::Relaxed);
            Box::new(|compositor: &mut Compositor, _ctx| {
                // remove the layer
                compositor.last_picker = compositor.pop();
            })
        };
        EventResult::Consumed(Some(callback))
    }

    /// A click marks the row it lands on, so the preview shows it, and a second
    /// click on that row opens it. The wheel walks the list without opening anything,
    /// and over the preview it scrolls the preview. The pointer merely moving is nobody's:
    /// taking it would repaint the screen at every motion.
    fn handle_mouse(&mut self, event: &MouseEvent, ctx: &mut Context) -> EventResult {
        if event.kind == MouseEventKind::Moved {
            return EventResult::Ignored(None);
        }

        let len = self.matcher.snapshot().matched_item_count();
        let lines = ctx.editor.config().scroll_lines.unsigned_abs() as u32;
        let on_preview = {
            let area = self.preview_area;
            event.row >= area.y
                && event.row < area.bottom()
                && event.column >= area.x
                && event.column < area.right()
        };

        // A click anywhere on an input line left of its right edge, the label
        // included, puts the focus there.
        let on_line = |area: &Rect| event.row == area.y && event.column < area.right();
        let on_cell = |area: &Rect| {
            event.row == area.y && event.column >= area.x && event.column < area.right()
        };
        let field = self
            .field_areas
            .iter()
            .find(|(_, area)| on_line(area))
            .map(|(index, _)| *index);
        let toggle = self.toggle_areas.iter().position(on_cell);
        let button = self.button_area.as_ref().is_some_and(on_cell);

        match event.kind {
            MouseEventKind::Down(MouseButton::Left) if button => {
                self.run_panel_action(ctx);
            }
            MouseEventKind::Down(MouseButton::Left) if toggle.is_some() => {
                self.flip_toggle(toggle.expect("just checked"));
            }
            MouseEventKind::Down(MouseButton::Left) if on_line(&self.query_area) => {
                self.focus = Focus::Query;
            }
            MouseEventKind::Down(MouseButton::Left) if field.is_some() => {
                self.focus = Focus::Field(field.expect("just checked"));
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let area = self.rows_area;
                let inside = event.row >= area.y
                    && event.row < area.bottom()
                    && event.column >= area.x
                    && event.column < area.right();
                let index = self.rows_offset + event.row.saturating_sub(area.y) as u32;
                if !inside || index >= len {
                    return EventResult::Consumed(None);
                }

                let now = Instant::now();
                let double = self
                    .last_click
                    .is_some_and(|(row, at)| row == index && now.duration_since(at) < DOUBLE_CLICK);
                self.cursor = index;
                if double {
                    self.last_click = None;
                    return self.accept(ctx);
                }
                self.last_click = Some((index, now));
            }
            MouseEventKind::ScrollDown if on_preview => self.scroll_preview(lines as isize),
            MouseEventKind::ScrollUp if on_preview => self.scroll_preview(-(lines as isize)),
            MouseEventKind::ScrollDown => {
                self.cursor = self.cursor.saturating_add(lines).min(len.saturating_sub(1));
            }
            MouseEventKind::ScrollUp => {
                self.cursor = self.cursor.saturating_sub(lines);
            }
            _ => {}
        }

        // Picker is a modal and should consume mouse events so clicks don't fall
        // through to the editor underneath
        EventResult::Consumed(None)
    }

    fn scroll_preview(&mut self, by: isize) {
        let lines = if self.preview_scroll.row == self.cursor {
            self.preview_scroll.lines
        } else {
            0
        };

        self.preview_scroll = PreviewScroll {
            row: self.cursor,
            lines: lines + by,
        };
    }

    fn prompt_handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        if let EventResult::Consumed(_) = self.prompt.handle_event(event, cx) {
            self.handle_prompt_change(matches!(event, Event::Paste(_)));
        }

        EventResult::Consumed(None)
    }

    fn panel_toggle_at(&self, event: KeyEvent) -> Option<usize> {
        if event.modifiers != KeyModifiers::ALT {
            return None;
        }

        let KeyCode::Char(pressed) = event.code else {
            return None;
        };

        self.panel_toggles
            .iter()
            .position(|toggle| toggle.key == pressed)
    }

    fn handle_prompt_change(&mut self, is_paste: bool) {
        // TODO: better track how the pattern has changed
        let line = self.prompt.line();
        let old_query = self.query.parse(line);
        if self.query == old_query {
            return;
        }
        // If the query has meaningfully changed, reset the cursor to the top of the results.
        self.cursor = 0;
        // Have nucleo reparse each changed column.
        for (i, column) in self
            .columns
            .iter()
            .filter(|column| column.filter)
            .enumerate()
        {
            let pattern = self
                .query
                .get(&column.name)
                .map(|f| &**f)
                .unwrap_or_default();
            let old_pattern = old_query
                .get(&column.name)
                .map(|f| &**f)
                .unwrap_or_default();
            // Fastlane: most columns will remain unchanged after each edit.
            if pattern == old_pattern {
                continue;
            }
            let (pattern, old_pattern) = if column.words {
                (whole_words(pattern), whole_words(old_pattern))
            } else {
                (pattern.to_string(), old_pattern.to_string())
            };
            let is_append = pattern.starts_with(&old_pattern);
            self.matcher.pattern.reparse(
                i,
                &pattern,
                CaseMatching::Smart,
                Normalization::Smart,
                is_append,
            );
        }
        // If this is a dynamic picker, notify the query hook that the primary
        // query might have been updated.
        self.refresh_dynamic_query(is_paste);
    }

    /// Re-run a dynamic query against the panel's current inputs. Called after the
    /// query line changes and after a panel field or switch changes.
    fn refresh_dynamic_query(&self, is_paste: bool) {
        self.send_dynamic_query(is_paste, false);
    }

    fn send_dynamic_query(&self, is_paste: bool, rerun: bool) {
        let Some(handler) = &self.dynamic_query_handler else {
            return;
        };

        let event = DynamicQueryChange {
            input: self.panel_input(),
            is_paste,
            rerun,
        };
        helix_event::send_blocking(handler, event);
    }

    /// Get (cached) preview for the currently selected item. If a document corresponding
    /// to the path is already open in the editor, it is used instead.
    fn get_preview<'picker, 'editor>(
        &'picker mut self,
        editor: &'editor Editor,
    ) -> Option<(Preview<'picker, 'editor>, Option<(usize, usize)>)> {
        let current = self.selection()?;
        let (path_or_id, range) = (self.file_fn.as_ref()?)(editor, current)?;

        match path_or_id {
            PathOrId::Path(path) => {
                if let Some(doc) = editor.document_by_path(path) {
                    return Some((Preview::EditorDocument(doc), range));
                }

                if self.preview_cache.contains_key(path) {
                    // NOTE: we use `HashMap::get_key_value` here instead of indexing so we can
                    // retrieve the `Arc<Path>` key. The `path` in scope here is a `&Path` and
                    // we can cheaply clone the key for the preview highlight handler.
                    let (path, preview) = self.preview_cache.get_key_value(path).unwrap();
                    if matches!(preview, CachedPreview::Document(doc) if doc.syntax().is_none()) {
                        helix_event::send_blocking(&self.preview_highlight_handler, path.clone());
                    }
                    return Some((Preview::Cached(preview), range));
                }

                let path: Arc<Path> = path.into();
                let preview = std::fs::metadata(&path)
                    .and_then(|metadata| {
                        if metadata.is_dir() {
                            let files = super::directory_content(&path, editor)?;
                            let file_names: Vec<_> = files
                                .iter()
                                .filter_map(|(file_path, is_dir)| {
                                    let name = file_path
                                        .strip_prefix(&path)
                                        .map(|p| Some(p.as_os_str()))
                                        .unwrap_or_else(|_| file_path.file_name())?
                                        .to_string_lossy();
                                    if *is_dir {
                                        Some((format!("{}/", name), true))
                                    } else {
                                        Some((name.into_owned(), false))
                                    }
                                })
                                .collect();
                            Ok(CachedPreview::Directory(file_names))
                        } else if metadata.is_file() {
                            if metadata.len() > MAX_FILE_SIZE_FOR_PREVIEW {
                                return Ok(CachedPreview::LargeFile);
                            }
                            let is_binary = std::fs::File::open(&path).and_then(|file| {
                                // Read up to 1kb to detect the content type
                                let n = file.take(1024).read_to_end(&mut self.read_buffer)?;
                                let is_binary = crate::is_binary(&self.read_buffer[..n]);
                                self.read_buffer.clear();
                                Ok(is_binary)
                            })?;
                            if is_binary {
                                return Ok(CachedPreview::Binary);
                            }
                            let mut doc = Document::open(
                                &path,
                                None,
                                false,
                                editor.config.clone(),
                                editor.syn_loader.clone(),
                            )
                            .or(Err(std::io::Error::new(
                                std::io::ErrorKind::NotFound,
                                "Cannot open document",
                            )))?;
                            let loader = editor.syn_loader.load();
                            if let Some(language_config) = doc.detect_language_config(&loader) {
                                doc.language = Some(language_config);
                                // Asynchronously highlight the new document
                                helix_event::send_blocking(
                                    &self.preview_highlight_handler,
                                    path.clone(),
                                );
                            }
                            Ok(CachedPreview::Document(Box::new(doc)))
                        } else {
                            Err(std::io::Error::new(
                                std::io::ErrorKind::NotFound,
                                "Neither a dir, nor a file",
                            ))
                        }
                    })
                    .unwrap_or(CachedPreview::NotFound);
                self.preview_cache.insert(path.clone(), preview);
                Some((Preview::Cached(&self.preview_cache[&path]), range))
            }
            PathOrId::Id(id) => {
                let doc = editor.documents.get(&id).unwrap();
                Some((Preview::EditorDocument(doc), range))
            }
        }
    }

    fn render_picker(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        let status = self.matcher.tick(10);
        let snapshot = self.matcher.snapshot();
        if status.changed {
            self.cursor = self
                .cursor
                .min(snapshot.matched_item_count().saturating_sub(1))
        }

        let text_style = cx.editor.theme.get("ui.text");
        let selected = cx.editor.theme.get("ui.text.focus");
        let highlight_style = cx.editor.theme.get("special").add_modifier(Modifier::BOLD);

        // -- Render the frame:
        // clear area
        let background = cx.editor.theme.get("ui.background");
        surface.clear_with(area, background);

        const BLOCK: Block<'_> = Block::bordered();

        // calculate the inner area inside the box
        let inner = BLOCK.inner(area);

        BLOCK.render(area, surface);

        if let Some(title) = &self.title {
            let width = area.width.saturating_sub(4) as usize;
            let title = format!(" {title} ");
            surface.set_stringn(area.x + 2, area.y, &title, width, text_style);
        }

        // On the border it costs no row of results. What does not fit is dropped
        // whole, never cut halfway through a key.
        if !self.hint.is_empty() {
            let width = area.width.saturating_sub(4) as usize;
            let mut hint = String::new();

            for item in self.hint {
                let next = if hint.is_empty() {
                    format!(" {item} ")
                } else {
                    format!("{hint} {item} ")
                };
                if next.chars().count() > width {
                    break;
                }
                hint = next;
            }

            let y = area.bottom().saturating_sub(1);
            surface.set_stringn(area.x + 2, y, &hint, width, text_style);
        }

        // -- Render the input bar:

        let count = format!(
            "{}{}/{}",
            if status.running || self.matcher.active_injectors() > 0 {
                "(running) "
            } else {
                ""
            },
            snapshot.matched_item_count(),
            snapshot.item_count(),
        );

        // A switch is two states a glance must tell apart. On wears the mode badge's
        // colour, which every theme that has one makes read in light and dark alike
        // (`ui.menu.selected` does not: a near-white grey in a light theme reads the
        // same as off). A theme without it gets reversed, which always reads.
        let toggle_on_style = cx
            .editor
            .theme
            .try_get("ui.statusline.normal")
            .unwrap_or_else(|| text_style.add_modifier(Modifier::REVERSED));
        let toggle_off_style = text_style.add_modifier(Modifier::DIM);
        let button_style = cx.editor.theme.get("ui.statusline");

        // A switch off with text in a box it hides may have something to say about
        // it, in front of the switches: hidden filters still narrow the results.
        let mut suffix: Vec<(Cow<str>, Style)> = Vec::new();
        let warning_style = cx.editor.theme.get("warning");
        for toggle in &self.panel_toggles {
            let Some(warning) = toggle.warning else {
                continue;
            };

            let hiding_text = self.panel_fields.iter().any(|field| {
                field.shown_by == Some(toggle.name) && !field.prompt.line().is_empty()
            });
            if !toggle.on && hiding_text {
                suffix.push((warning.into(), warning_style));
            }
        }
        let first_toggle = suffix.len();

        // The switches sit between the query and the match count, at the right end,
        // each padded inside its own colour so it reads as a button.
        for toggle in &self.panel_toggles {
            let style = if toggle.on {
                toggle_on_style
            } else {
                toggle_off_style
            };

            suffix.push((format!(" {} ", toggle.label).into(), style));
        }

        suffix.push((count.as_str().into(), text_style));

        let area = inner.clip_left(1).with_height(1);
        let suffix_width = suffix_width(&suffix);
        let line_area = area.clip_right(suffix_width);
        self.query_area = line_area;

        // render the prompt first since it will clear its background
        self.prompt.render(line_area, surface, cx);

        let mut x = area.right().saturating_sub(suffix_width);
        self.toggle_areas.clear();

        for (index, (text, style)) in suffix.iter().enumerate() {
            surface.set_stringn(x, area.y, text, text.len(), *style);
            let width = text.chars().count() as u16;

            // Before the switches may sit a warning, after them the match count.
            if (first_toggle..first_toggle + self.panel_toggles.len()).contains(&index) {
                self.toggle_areas.push(Rect::new(x, area.y, width, 1));
            }

            x += width + SUFFIX_GAP;
        }

        // -- The boxes on screen, one per line under the query, their inputs lined
        // up after the widest label.
        let label_width = self
            .panel_fields
            .iter()
            .map(|field| field.label.chars().count() as u16)
            .max()
            .unwrap_or_default();
        self.field_areas.clear();
        self.button_area = None;
        let shown = self.shown_fields();

        for (line_index, field_index) in shown.iter().enumerate() {
            let field = &mut self.panel_fields[*field_index];
            let line = Rect {
                y: area.y + 1 + line_index as u16,
                ..area
            };
            surface.set_stringn(
                line.x,
                line.y,
                field.label,
                label_width as usize,
                toggle_off_style,
            );

            let mut input_area = line.clip_left(label_width + 2);

            // The button sits at the right end, lined up with the match count above
            // it, in the statusline's grey so it does not read as a switch. It says
            // ⏎ because Enter in its box presses it.
            if let Some(button) = field.button {
                let button = format!(" ⏎ {button} ");
                let width = button.chars().count() as u16;
                let button_area =
                    Rect::new(line.right().saturating_sub(width + 1), line.y, width, 1);
                surface.set_stringn(
                    button_area.x,
                    button_area.y,
                    &button,
                    width as usize,
                    button_style,
                );
                self.button_area = Some(button_area);
                input_area = input_area.clip_right(width + 2);
            }

            field.prompt.render(input_area, surface, cx);
            self.field_areas.push((*field_index, input_area));
        }

        let input_lines = 1 + shown.len() as u16;

        // -- Separator
        let sep_style = cx.editor.theme.get("ui.background.separator");
        let borders = BorderType::line_symbols(BorderType::Plain);
        for x in inner.left()..inner.right() {
            if let Some(cell) = surface.get_mut(x, inner.y + input_lines) {
                cell.set_symbol(borders.horizontal).set_style(sep_style);
            }
        }

        // -- Render the contents:
        // subtract area of the input lines and the separator from top
        let inner = inner.clip_top(input_lines + 1);
        let rows = inner.height.saturating_sub(self.header_height()) as u32;
        let offset = self.cursor - (self.cursor % std::cmp::max(1, rows));
        let cursor = self.cursor.saturating_sub(offset);
        let end = offset
            .saturating_add(rows)
            .min(snapshot.matched_item_count());
        self.rows_area = inner.clip_top(self.header_height());
        self.rows_offset = offset;
        let mut indices = Vec::new();
        let mut matcher = MATCHER.lock();
        matcher.config = Config::DEFAULT;
        if self.file_fn.is_some() {
            matcher.config.set_match_paths()
        }

        // When what is typed searches a hidden column, the shown columns mark what it
        // matched in their own text.
        let primary = &self.columns[self.primary_column];
        let searched = (primary.hidden && primary.filter).then(|| {
            self.columns[..self.primary_column]
                .iter()
                .filter(|column| column.filter)
                .count()
        });
        let options = snapshot.matched_items(offset..end).map(|item| {
            let mut widths = self.widths.iter_mut();
            let mut matcher_index = 0;

            Row::new(self.columns.iter().map(|column| {
                if column.hidden {
                    if column.filter {
                        matcher_index += 1;
                    }
                    return Cell::default();
                }

                let Some(Constraint::Length(max_width)) = widths.next() else {
                    unreachable!();
                };
                let mut cell = column.format(item.data, &self.editor_data);
                let width = if column.filter {
                    let own = snapshot.pattern().column_pattern(matcher_index);
                    let pattern = match searched {
                        Some(searched) if own.atoms.is_empty() => {
                            snapshot.pattern().column_pattern(searched)
                        }
                        _ => own,
                    };
                    pattern.indices(
                        item.matcher_columns[matcher_index].slice(..),
                        &mut matcher,
                        &mut indices,
                    );
                    indices.sort_unstable();
                    indices.dedup();
                    let mut indices = indices.drain(..);
                    let mut next_highlight_idx = indices.next().unwrap_or(u32::MAX);
                    let mut span_list = Vec::new();
                    let mut current_span = String::new();
                    let mut current_style = Style::default();
                    let mut grapheme_idx = 0u32;
                    let mut width = 0;

                    let spans: &[Span] =
                        cell.content.lines.first().map_or(&[], |it| it.0.as_slice());
                    for span in spans {
                        // this looks like a bug on first glance, we are iterating
                        // graphemes but treating them as char indices. The reason that
                        // this is correct is that nucleo will only ever consider the first char
                        // of a grapheme (and discard the rest of the grapheme) so the indices
                        // returned by nucleo are essentially grapheme indecies
                        for grapheme in span.content.graphemes(true) {
                            let style = if grapheme_idx == next_highlight_idx {
                                next_highlight_idx = indices.next().unwrap_or(u32::MAX);
                                span.style.patch(highlight_style)
                            } else {
                                span.style
                            };
                            if style != current_style {
                                if !current_span.is_empty() {
                                    span_list.push(Span::styled(current_span, current_style))
                                }
                                current_span = String::new();
                                current_style = style;
                            }
                            current_span.push_str(grapheme);
                            grapheme_idx += 1;
                        }
                        width += span.width();
                    }

                    span_list.push(Span::styled(current_span, current_style));
                    cell = Cell::from(Spans::from(span_list));
                    matcher_index += 1;
                    width
                } else {
                    cell.content
                        .lines
                        .first()
                        .map(|line| line.width())
                        .unwrap_or_default()
                };

                if width as u16 > *max_width {
                    *max_width = width as u16;
                }

                cell
            }))
        });

        // A path keeps its tail so the file name survives; a column of plain text
        // keeps its head, where the reader starts.
        let truncate_start: Vec<bool> = self
            .columns
            .iter()
            .map(|column| self.truncate_start && column.truncate_start)
            .collect();

        let mut table = Table::new(options)
            .style(text_style)
            .highlight_style(selected)
            .highlight_symbol(" > ")
            .column_spacing(1)
            .widths(&self.widths);

        // -- Header
        if self.columns.len() > 1 {
            let active_column = self.query.active_column(self.prompt.position());
            let header_style = cx.editor.theme.get("ui.picker.header");
            let header_column_style = cx.editor.theme.get("ui.picker.header.column");

            table = table.header(
                Row::new(self.columns.iter().map(|column| {
                    if column.hidden {
                        Cell::default()
                    } else {
                        let style =
                            if active_column.is_some_and(|name| Arc::ptr_eq(name, &column.name)) {
                                cx.editor.theme.get("ui.picker.header.column.active")
                            } else {
                                header_column_style
                            };

                        Cell::from(Span::styled(Cow::from(&*column.name), style))
                    }
                }))
                .style(header_style),
            );
        }

        use tui::widgets::TableState;

        table.render_table(
            inner,
            surface,
            &mut TableState {
                offset: 0,
                selected: Some(cursor as usize),
            },
            &truncate_start,
        );
    }

    fn render_preview(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        // -- Render the frame:
        // clear area
        let background = cx.editor.theme.get("ui.background");
        let text = cx.editor.theme.get("ui.text");
        let directory = cx.editor.theme.get("ui.text.directory");
        surface.clear_with(area, background);

        const BLOCK: Block<'_> = Block::bordered();

        // calculate the inner area inside the box
        let inner = BLOCK.inner(area);
        // 1 column gap on either side
        let margin = Margin::horizontal(1);
        let inner = inner.inner(margin);
        BLOCK.render(area, surface);

        let scrolled = if self.preview_scroll.row == self.cursor {
            self.preview_scroll.lines
        } else {
            0
        };
        if let Some((preview, range)) = self.get_preview(cx.editor) {
            let doc = match preview.document() {
                Some(doc)
                    if range.is_none_or(|(start, end)| {
                        start <= end && end <= doc.text().len_lines()
                    }) =>
                {
                    doc
                }
                _ => {
                    if let Some(dir_content) = preview.dir_content() {
                        for (i, (path, is_dir)) in
                            dir_content.iter().take(inner.height as usize).enumerate()
                        {
                            let style = if *is_dir { directory } else { text };
                            surface.set_stringn(
                                inner.x,
                                inner.y + i as u16,
                                path,
                                inner.width as usize,
                                style,
                            );
                        }
                        return;
                    }

                    let alt_text = preview.placeholder();
                    let x = inner.x + inner.width.saturating_sub(alt_text.len() as u16) / 2;
                    let y = inner.y + inner.height / 2;
                    surface.set_stringn(x, y, alt_text, inner.width as usize, text);
                    return;
                }
            };

            let mut offset = ViewPosition::default();
            if let Some((start_line, end_line)) = range {
                let height = end_line - start_line;
                let text = doc.text().slice(..);
                let start = text.line_to_char(start_line);
                let middle = text.line_to_char(start_line + height / 2);
                if height < inner.height as usize {
                    let text_fmt = doc.text_format(inner.width, None);
                    let annotations = TextAnnotations::default();
                    (offset.anchor, offset.vertical_offset) = char_idx_at_visual_offset(
                        text,
                        middle,
                        // align to middle
                        -(inner.height as isize / 2),
                        0,
                        &text_fmt,
                        &annotations,
                    );
                    if start < offset.anchor {
                        offset.anchor = start;
                        offset.vertical_offset = 0;
                    }
                } else {
                    offset.anchor = start;
                }
            }

            if scrolled != 0 {
                let text = doc.text().slice(..);
                let last = text.len_lines().saturating_sub(1) as isize;
                let line = text.char_to_line(offset.anchor) as isize + scrolled;
                offset.anchor = text.line_to_char(line.clamp(0, last) as usize);
                offset.vertical_offset = 0;
            }

            let loader = cx.editor.syn_loader.load();
            let config = cx.editor.config();

            let syntax_highlighter =
                EditorView::doc_syntax_highlighter(doc, offset.anchor, area.height, &loader);
            let mut overlay_highlights = Vec::new();
            if doc
                .language_config()
                .and_then(|config| config.rainbow_brackets)
                .unwrap_or(config.rainbow_brackets)
            {
                if let Some(overlay) = EditorView::doc_rainbow_highlights(
                    doc,
                    offset.anchor,
                    area.height,
                    &cx.editor.theme,
                    &loader,
                ) {
                    overlay_highlights.push(overlay);
                }
            }

            EditorView::doc_diagnostics_highlights_into(
                doc,
                &cx.editor.theme,
                &mut overlay_highlights,
            );

            let mut decorations = DecorationManager::default();

            if let Some((start, end)) = range {
                let style = cx
                    .editor
                    .theme
                    .try_get("ui.highlight")
                    .unwrap_or_else(|| cx.editor.theme.get("ui.selection"));
                let draw_highlight = move |renderer: &mut TextRenderer, pos: LinePos| {
                    if (start..=end).contains(&pos.doc_line) {
                        let area = Rect::new(
                            renderer.viewport.x,
                            pos.visual_line,
                            renderer.viewport.width,
                            1,
                        );
                        renderer.set_style(area, style)
                    }
                };
                decorations.add_decoration(draw_highlight);
            }

            render_document(
                surface,
                inner,
                doc,
                offset,
                // TODO: compute text annotations asynchronously here (like inlay hints)
                &TextAnnotations::default(),
                syntax_highlighter,
                overlay_highlights,
                &cx.editor.theme,
                decorations,
            );
        }
    }
}

impl<I: 'static + Send + Sync, D: 'static + Send + Sync> Component for Picker<I, D> {
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        // +---------+ +---------+
        // |prompt   | |preview  |
        // +---------+ |         |
        // |picker   | |         |
        // |         | |         |
        // +---------+ +---------+

        let render_preview =
            self.show_preview && self.file_fn.is_some() && area.width > MIN_AREA_WIDTH_FOR_PREVIEW;

        let picker_width = if render_preview {
            area.width / 2
        } else {
            area.width
        };

        let picker_area = area.with_width(picker_width);
        self.render_picker(picker_area, surface, cx);

        if render_preview {
            let preview_area = area.clip_left(picker_width);
            self.preview_area = preview_area;
            self.render_preview(preview_area, surface, cx);
        } else {
            self.preview_area = Rect::default();
        }
    }

    fn handle_event(&mut self, event: &Event, ctx: &mut Context) -> EventResult {
        // TODO: keybinds for scrolling preview

        let key_event = match event {
            Event::Key(event) => *event,
            Event::Paste(..) => return self.input_handle_event(event, ctx),
            Event::Resize(..) => return EventResult::Consumed(None),
            Event::Mouse(event) => return self.handle_mouse(event, ctx),
            _ => return EventResult::Ignored(None),
        };

        // A panel's switches are flipped with Alt + their own key, whichever line the
        // cursor is on.
        if let Some(index) = self.panel_toggle_at(key_event) {
            self.flip_toggle(index);
            return EventResult::Consumed(None);
        }

        // Tab walks the boxes and switches when the picker has any; without them it
        // keeps stepping through the results, as it always did.
        let has_stops = !self.panel_toggles.is_empty() || !self.shown_fields().is_empty();

        match key_event {
            shift!(Tab) if has_stops => {
                self.focus_by(Direction::Backward);
            }
            key!(Tab) if has_stops => {
                self.focus_by(Direction::Forward);
            }
            key!(' ') if matches!(self.focus, Focus::Toggle(_)) => {
                let Focus::Toggle(index) = self.focus else {
                    unreachable!("just checked");
                };
                self.flip_toggle(index);
            }
            shift!(Tab) | key!(Up) | ctrl!('p') => {
                self.move_by(1, Direction::Backward);
            }
            key!(Tab) | key!(Down) | ctrl!('n') => {
                self.move_by(1, Direction::Forward);
            }
            key!(PageDown) | ctrl!('d') => {
                self.page_down();
            }
            key!(PageUp) | ctrl!('u') => {
                self.page_up();
            }
            key!(Home) => {
                self.to_start();
            }
            key!(End) => {
                self.to_end();
            }
            key!(Esc) | ctrl!('c') => return self.close(),
            ctrl!('f')
            | KeyEvent {
                code: KeyCode::Char('f'),
                modifiers: KeyModifiers::SUPER,
            } if self.widen.is_some() => {
                let query = self.prompt.line().clone();
                let widen = self.widen.take().unwrap();
                return EventResult::Consumed(Some(Box::new(move |compositor, cx| {
                    compositor.pop();
                    widen(compositor, cx, query);
                })));
            }
            alt!(Enter) => {
                if let Some(option) = self.selection() {
                    (self.callback_fn)(ctx, option, self.default_action);
                }
            }
            // In a box with a button, Enter presses the button, as its ⏎ says.
            key!(Enter)
                if matches!(
                    self.focus,
                    Focus::Field(index) if self.panel_fields[index].button.is_some()
                ) =>
            {
                self.run_panel_action(ctx);
            }
            key!(Enter) => {
                // If the prompt has a history completion and is empty, use enter to accept
                // that completion
                if let Some(completion) = self
                    .prompt
                    .first_history_completion(ctx.editor)
                    .filter(|_| self.prompt.line().is_empty())
                {
                    // The percent character is used by the query language and needs to be
                    // escaped with a backslash.
                    let completion = if completion.contains('%') {
                        completion.replace('%', "\\%")
                    } else {
                        completion.into_owned()
                    };
                    self.prompt.set_line(completion, ctx.editor);

                    // Inserting from the history register is a paste.
                    self.handle_prompt_change(true);
                } else {
                    return self.accept(ctx);
                }
            }
            ctrl!('s') => {
                if let Some(option) = self.selection() {
                    (self.callback_fn)(ctx, option, Action::HorizontalSplit);
                }
                return self.close();
            }
            ctrl!('v') => {
                if let Some(option) = self.selection() {
                    (self.callback_fn)(ctx, option, Action::VerticalSplit);
                }
                return self.close();
            }
            ctrl!('t') => {
                self.toggle_preview();
            }
            alt!('a') if self.panel_action.is_some() => {
                self.run_panel_action(ctx);
            }
            _ => {
                self.input_handle_event(event, ctx);
            }
        }

        EventResult::Consumed(None)
    }

    fn cursor(&self, area: Rect, editor: &Editor) -> (Option<Position>, CursorKind) {
        let block = Block::bordered();
        // calculate the inner area inside the box
        let inner = block.inner(area);

        // prompt area
        let render_preview =
            self.show_preview && self.file_fn.is_some() && area.width > MIN_AREA_WIDTH_FOR_PREVIEW;

        let picker_width = if render_preview {
            area.width / 2
        } else {
            area.width
        };
        match self.focus {
            // On a switch the ordinary block cursor sits on its label, so "where am
            // I" is answered by the same thing everywhere.
            Focus::Toggle(index) => {
                let position = self
                    .toggle_areas
                    .get(index)
                    // Past the padding, on the label itself.
                    .map(|area| Position::new(area.y as usize, area.x as usize + 1));
                (position, CursorKind::Block)
            }
            Focus::Field(index) => {
                let area = self
                    .field_areas
                    .iter()
                    .find(|(field, _)| *field == index)
                    .map(|(_, area)| *area);

                match area {
                    Some(area) => self.panel_fields[index].prompt.cursor(area, editor),
                    None => (None, CursorKind::Hidden),
                }
            }
            Focus::Query => {
                let area = inner.clip_left(1).with_height(1).with_width(picker_width);
                self.prompt.cursor(area, editor)
            }
        }
    }

    fn required_size(&mut self, (width, height): (u16, u16)) -> Option<(u16, u16)> {
        let input_lines = 1 + self.shown_fields().len() as u16;
        self.completion_height = height.saturating_sub(3 + input_lines + self.header_height());
        Some((width, height))
    }

    fn id(&self) -> Option<&'static str> {
        Some(ID)
    }
}
impl<T: 'static + Send + Sync, D> Drop for Picker<T, D> {
    fn drop(&mut self) {
        // ensure we cancel any ongoing background threads streaming into the picker
        self.version.fetch_add(1, atomic::Ordering::Relaxed);
    }
}

/// Columns between the items at the right end of the query line. A switch carries
/// its own padding, so one is enough.
const SUFFIX_GAP: u16 = 1;

/// Width of a rendered input-line suffix: the items themselves, the gaps between
/// them and one column at the right edge.
fn suffix_width(suffix: &[(Cow<str>, Style)]) -> u16 {
    if suffix.is_empty() {
        return 0;
    }

    let text: usize = suffix.iter().map(|(text, _)| text.chars().count()).sum();

    text as u16 + SUFFIX_GAP * (suffix.len() as u16 - 1) + 1
}

type PickerCallback<T> = Box<dyn Fn(&mut Context, &T, Action)>;

type PanelCallback<T> = Box<dyn Fn(&mut Context, &PanelInput, &[&T])>;

/// Asks nucleo for each word of `query` as it was typed, together and in order: a
/// leading `'` is its mark for that. A word that already carries a mark of its own
/// (`^`, `'`, `!`, `$` or a `\` escape) is left as it is.
pub fn whole_words(query: &str) -> String {
    let mut out = String::with_capacity(query.len() + 8);
    let mut word_start = true;
    for c in query.chars() {
        if word_start && !c.is_whitespace() {
            if !matches!(c, '^' | '\'' | '!' | '\\') {
                out.push('\'');
            }
            word_start = false;
        }
        if c.is_whitespace() {
            word_start = true;
        }
        out.push(c);
    }
    out
}
