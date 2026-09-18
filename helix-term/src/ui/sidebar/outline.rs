//! The outline: what the file being edited defines — its functions and methods, or every
//! kind of definition when asked for them — listed under the tree in the Files tab or in
//! its own column beside it, in the order they appear or by name. A click goes to one. The list comes from the language server when one is running, which
//! knows every kind of thing the file defines; otherwise from the file's syntax tree,
//! which needs nothing installed. Either way it follows what is typed.

use std::collections::HashSet;
use std::time::Duration;

use helix_core::syntax::config::LanguageServerFeature;
use helix_core::Selection;
use helix_lsp::util::lsp_range_to_range;
use helix_lsp::{lsp, OffsetEncoding};
use helix_view::graphics::{Modifier, Rect, Style};
use helix_view::{align_view, Align, DocumentId, Editor, Theme};
use tui::buffer::Buffer as Surface;

use super::commit_layout::{share_at, share_at_column, side_panes, stacked_panes};
use super::entries::{Row, RowPaint, SymbolRow};
use super::list::List;
use super::tab::{Activation, Message, Outcome, TabContext, TabView};
use crate::commands::lsp::display_symbol_kind;
use crate::commands::push_jump;
use crate::commands::syntax::{document_symbols, Symbol};
use crate::ui::{editor, panel_width};

const STATE_FILE: &str = "sidebar-outline";

/// How long the file rests after a change before the language server is asked again: a
/// burst of typing asks once, at the end.
const ASK_DELAY: Duration = Duration::from_millis(300);

/// A document at a revision: what a list of definitions was read from.
type Key = (DocumentId, usize);

/// A definition as the language server said it, before its positions are turned into
/// characters of the document, which only the main thread holds.
pub struct Said {
    kind: &'static str,
    name: String,
    range: lsp::Range,
    jump: lsp::Position,
    encoding: OffsetEncoding,
}

/// Whether the outline is shown, what it lists, how it is ordered and where it sits
/// beside the tree, remembered between sessions.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct OutlineLayout {
    pub shown: bool,
    pub by_name: bool,
    /// Whether it lists only what is called — functions, methods, constructors — rather
    /// than every definition the file holds.
    pub only_functions: bool,
    /// Whether it sits in its own column beside the tree rather than under it.
    pub beside: bool,
    /// Proportion of the usable rows given to the tree, in thousandths.
    tree_share: u16,
    /// Proportion of the usable columns given to the tree when the outline is beside it.
    tree_columns: u16,
}

impl Default for OutlineLayout {
    fn default() -> Self {
        Self {
            shown: false,
            by_name: false,
            only_functions: true,
            beside: false,
            tree_share: 600,
            tree_columns: 500,
        }
    }
}

impl OutlineLayout {
    pub fn load() -> anyhow::Result<Self> {
        let layout: Option<Self> = panel_width::load_state(STATE_FILE)?;
        let layout = layout.unwrap_or_default();
        anyhow::ensure!(
            layout.tree_share <= 1000 && layout.tree_columns <= 1000,
            "invalid outline pane proportion"
        );
        Ok(layout)
    }

    pub fn save(&self) -> anyhow::Result<()> {
        panel_width::save_state(STATE_FILE, self)
    }
}

pub struct Outline {
    layout: OutlineLayout,
    rows: Vec<Row>,
    list: List,
    /// The definitions in the order they appear in the file.
    symbols: Vec<Symbol>,
    /// Which document, at which revision, the definitions were read from.
    read_from: Option<Key>,
    /// The revision the language server was asked about, or is about to be, until its
    /// answer lands; an answer for any other is dropped.
    wanted: Option<Key>,
    /// Whether the definitions came from a language server or the syntax tree.
    from_server: bool,
    /// Whether that document has a syntax tree at all.
    has_syntax: bool,
    /// Whether the keys are the outline's rather than the tree's.
    pub focused: bool,
    /// The row of the definition the editor's cursor is inside, the innermost.
    current: Option<usize>,
}

impl Outline {
    pub fn new() -> (Self, Option<String>) {
        let (layout, error) = match OutlineLayout::load() {
            Ok(layout) => (layout, None),
            Err(err) => {
                log::error!("Could not read the outline layout: {err:#}");
                (
                    OutlineLayout::default(),
                    Some(format!("Could not read the outline layout: {err:#}")),
                )
            }
        };
        let outline = Self {
            layout,
            rows: Vec::new(),
            list: List::default(),
            symbols: Vec::new(),
            read_from: None,
            wanted: None,
            from_server: false,
            has_syntax: false,
            focused: false,
            current: None,
        };
        (outline, error)
    }

    pub fn shown(&self) -> bool {
        self.layout.shown
    }

    pub fn set_shown(&mut self, shown: bool) {
        self.layout.shown = shown;
        if !shown {
            self.focused = false;
        }
        self.remember();
    }

    pub fn by_name(&self) -> bool {
        self.layout.by_name
    }

    pub fn beside(&self) -> bool {
        self.layout.beside
    }

    /// Puts the outline in its own column beside the tree, or back under it.
    pub fn set_beside(&mut self, beside: bool) {
        self.layout.beside = beside;
        self.remember();
    }

    pub fn only_functions(&self) -> bool {
        self.layout.only_functions
    }

    /// Lists only functions and methods, or every definition the file holds.
    pub fn toggle_kinds(&mut self, editor: &mut Editor) {
        self.layout.only_functions = !self.layout.only_functions;
        self.rebuild(editor);
        self.remember();
    }

    /// Lists the definitions by name, or back in the order they appear in the file.
    pub fn toggle_sort(&mut self, editor: &mut Editor) {
        self.layout.by_name = !self.layout.by_name;
        self.rebuild(editor);
        self.remember();
    }

    /// Writes the layout down; a failure is logged, never in the way of the outline.
    fn remember(&self) {
        if let Err(err) = self.save() {
            log::error!("Could not remember the outline layout: {err:#}");
        }
    }

    /// What the header of the pane says about the order, and what a click on it does.
    pub fn sort_label(&self) -> &'static str {
        if self.layout.by_name {
            "by name"
        } else {
            "by position"
        }
    }

    pub fn panes(&self, area: Rect) -> Option<[Rect; 2]> {
        if self.layout.beside {
            side_panes(area, self.layout.tree_columns)
        } else {
            stacked_panes(area, self.layout.tree_share)
        }
    }

    /// Drags the rule between the panes to the pointer: the row it is on when they are
    /// stacked, the column when the outline is beside the tree.
    pub fn resize_split(&mut self, area: Rect, row: u16, column: u16) {
        if self.layout.beside {
            if let Some(share) = share_at_column(area, column) {
                self.layout.tree_columns = share;
            }
        } else if let Some(share) = share_at(area, row) {
            self.layout.tree_share = share;
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        self.layout.save()
    }

    /// Whether row `index` is the definition the editor's cursor is inside.
    pub fn is_current(&self, index: usize) -> bool {
        self.current == Some(index)
    }

    /// Reads the definitions again when the file being edited changed, or was edited,
    /// and marks the one the cursor is inside; run at every render while on screen.
    pub fn sync(&mut self, editor: &mut Editor, keys_here: bool) {
        let doc = doc_mut!(editor);
        let key = (doc.id(), doc.get_current_revision());
        if self.read_from != Some(key) && self.wanted != Some(key) {
            let served = doc
                .language_servers_with_feature(LanguageServerFeature::DocumentSymbols)
                .next()
                .is_some();
            if !served {
                self.wanted = None;
                self.read(editor);
                self.read_from = Some(key);
                self.from_server = false;
            } else {
                // Another file: the syntax tree says what it can right away, and the
                // server's word replaces it when it comes. The same file edited keeps
                // its list until then, rather than flicker between two shapes.
                if self.read_from.map(|(id, _)| id) != Some(key.0) {
                    self.read(editor);
                    self.read_from = Some(key);
                    self.from_server = false;
                }
                self.wanted = Some(key);
                super::later(ASK_DELAY, move |sidebar, editor| {
                    if sidebar.outline.wanted == Some(key) {
                        sidebar.outline.ask(editor, key);
                    }
                });
            }
        }
        let (view, doc) = current_ref!(editor);
        let text = doc.text().slice(..);
        let cursor = doc.selection(view.id).primary().cursor(text);
        let current = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| match row {
                Row::Symbol(symbol) if symbol.start <= cursor && cursor < symbol.end => {
                    Some((index, symbol.end - symbol.start))
                }
                _ => None,
            })
            .min_by_key(|(_, len)| *len)
            .map(|(index, _)| index);
        if current != self.current {
            self.current = current;
            // Without the keys the outline follows the cursor, as the tree follows the
            // file; with them the cursor in the list is yours.
            if let Some(index) = current.filter(|_| !keys_here) {
                self.list.select(index);
            }
        }
    }

    fn read(&mut self, editor: &mut Editor) {
        let loader = editor.syn_loader.load();
        let doc = doc!(editor);
        self.has_syntax = doc.syntax().is_some();
        let symbols = document_symbols(doc, &loader);
        self.take(editor, symbols);
    }

    fn take(&mut self, editor: &mut Editor, mut symbols: Vec<Symbol>) {
        symbols.sort_by_key(|symbol| (symbol.start, std::cmp::Reverse(symbol.end)));
        self.symbols = symbols;
        self.current = None;
        self.rebuild(editor);
    }

    /// Asks the document's language servers for its definitions, off the main thread;
    /// what they say lands back in [`Outline::landed`].
    fn ask(&mut self, editor: &mut Editor, key: Key) {
        let Some(doc) = editor.documents.get(&key.0) else {
            self.wanted = None;
            return;
        };
        let mut seen = HashSet::new();
        let requests: Vec<_> = doc
            .language_servers_with_feature(LanguageServerFeature::DocumentSymbols)
            .filter(|server| seen.insert(server.id()))
            .filter_map(|server| {
                let request = server.document_symbols(doc.identifier())?;
                Some((server.offset_encoding(), request))
            })
            .collect();
        if requests.is_empty() {
            self.wanted = None;
            self.read(editor);
            self.read_from = Some(key);
            self.from_server = false;
            return;
        }
        tokio::spawn(async move {
            let mut said = Vec::new();
            let mut answered = false;
            for (encoding, request) in requests {
                match request.await {
                    Ok(Some(response)) => {
                        answered = true;
                        flatten(response, encoding, &mut said);
                    }
                    Ok(None) => answered = true,
                    Err(err) => log::error!("Error requesting document symbols: {err}"),
                }
            }
            crate::job::dispatch(move |editor, compositor| {
                let Some(view) = compositor.find::<editor::EditorView>() else {
                    return;
                };
                view.sidebar
                    .outline_landed(editor, key, answered.then_some(said));
            })
            .await;
        });
    }

    /// What the language server said about `key`: the definitions, or nothing when no
    /// server answered, in which case the syntax tree is read instead.
    pub fn landed(&mut self, editor: &mut Editor, key: Key, said: Option<Vec<Said>>) {
        if self.wanted != Some(key) {
            return;
        }
        self.wanted = None;
        let Some(doc) = editor.documents.get(&key.0) else {
            return;
        };
        let Some(said) = said else {
            self.read(editor);
            self.read_from = Some(key);
            self.from_server = false;
            return;
        };
        let text = doc.text();
        let symbols = said
            .into_iter()
            .filter_map(|symbol| {
                let range = lsp_range_to_range(text, symbol.range, symbol.encoding)?;
                let jump = lsp_range_to_range(
                    text,
                    lsp::Range::new(symbol.jump, symbol.jump),
                    symbol.encoding,
                )
                .map_or(range.from(), |at| at.from());
                Some(Symbol {
                    kind: symbol.kind,
                    name: symbol.name,
                    start: range.from(),
                    end: range.to(),
                    jump,
                    line: text.char_to_line(range.from()),
                })
            })
            .collect();
        self.has_syntax = true;
        self.from_server = true;
        self.read_from = Some(key);
        self.take(editor, symbols);
    }
}

/// Lays a server's answer out flat, a nested symbol's children after it: the rows nest
/// them again by where they sit in the file.
fn flatten(response: lsp::DocumentSymbolResponse, encoding: OffsetEncoding, out: &mut Vec<Said>) {
    fn nested(symbol: lsp::DocumentSymbol, encoding: OffsetEncoding, out: &mut Vec<Said>) {
        out.push(Said {
            kind: display_symbol_kind(symbol.kind),
            name: symbol.name,
            range: symbol.range,
            jump: symbol.selection_range.start,
            encoding,
        });
        for child in symbol.children.into_iter().flatten() {
            nested(child, encoding, out);
        }
    }
    match response {
        lsp::DocumentSymbolResponse::Flat(symbols) => {
            for symbol in symbols {
                out.push(Said {
                    kind: display_symbol_kind(symbol.kind),
                    name: symbol.name,
                    range: symbol.location.range,
                    jump: symbol.location.range.start,
                    encoding,
                });
            }
        }
        lsp::DocumentSymbolResponse::Nested(symbols) => {
            for symbol in symbols {
                nested(symbol, encoding, out);
            }
        }
    }
}

/// Whether a definition is something that is called: what the outline lists when it is
/// narrowed to functions and methods, a constructor being a method under another name.
fn is_callable(kind: &str) -> bool {
    matches!(kind, "function" | "method" | "construct")
}

/// Lays `symbols`, in the file's order, out as rows: each one indented by how many
/// definitions it sits inside.
fn nested_rows(symbols: &[&Symbol]) -> Vec<Row> {
    let mut open: Vec<usize> = Vec::new();
    symbols
        .iter()
        .map(|symbol| {
            while open.last().is_some_and(|end| *end <= symbol.start) {
                open.pop();
            }
            let depth = open.len();
            open.push(symbol.end);
            Row::Symbol(symbol_row(symbol, depth))
        })
        .collect()
}

/// Lays `symbols` out by name, flat: a name reads the same wherever it was defined.
fn named_rows(symbols: &[&Symbol]) -> Vec<Row> {
    let mut sorted: Vec<&Symbol> = symbols.to_vec();
    sorted.sort_by_cached_key(|symbol| (symbol.name.to_lowercase(), symbol.start));
    sorted
        .into_iter()
        .map(|symbol| Row::Symbol(symbol_row(symbol, 0)))
        .collect()
}

fn symbol_row(symbol: &Symbol, depth: usize) -> SymbolRow {
    SymbolRow {
        name: symbol.name.clone(),
        kind: symbol.kind,
        depth,
        start: symbol.start,
        end: symbol.end,
        jump: symbol.jump,
    }
}

impl TabView for Outline {
    fn label(&self) -> String {
        "Outline".to_string()
    }

    fn rows(&self) -> &[Row] {
        &self.rows
    }

    fn list(&self) -> &List {
        &self.list
    }

    fn list_mut(&mut self) -> &mut List {
        &mut self.list
    }

    fn empty_message(&self) -> Option<Message> {
        let text = if !self.has_syntax {
            "no outline for this file"
        } else if self.layout.only_functions && !self.symbols.is_empty() {
            "no functions or methods here"
        } else {
            "nothing defined here"
        };
        Some(Message {
            text: text.to_string(),
            is_error: false,
        })
    }

    /// Lays the rows out from the definitions held; the cursor stays on its definition,
    /// found by name and kind, when the order or the file changed under it.
    fn rebuild(&mut self, _editor: &mut Editor) {
        let selected = match self.rows.get(self.list.cursor) {
            Some(Row::Symbol(symbol)) => Some((symbol.name.clone(), symbol.kind)),
            _ => None,
        };
        let listed: Vec<&Symbol> = self
            .symbols
            .iter()
            .filter(|symbol| !self.layout.only_functions || is_callable(symbol.kind))
            .collect();
        self.rows = if self.layout.by_name {
            named_rows(&listed)
        } else {
            nested_rows(&listed)
        };
        self.list.set_len(self.rows.len());
        let found = selected.and_then(|(name, kind)| {
            self.rows.iter().position(|row| {
                matches!(row, Row::Symbol(symbol) if symbol.name == name && symbol.kind == kind)
            })
        });
        if let Some(index) = found {
            self.list.select(index);
        }
    }

    fn refresh(&mut self, cx: &mut TabContext) {
        self.read_from = None;
        self.wanted = None;
        self.sync(cx.editor, self.focused);
    }

    /// Goes to the definition under the cursor, and the typing goes there with it, by a
    /// click or by Enter.
    fn open(&mut self, cx: &mut TabContext, _how: Activation) -> Outcome {
        let Some(Row::Symbol(symbol)) = self.rows.get(self.list.cursor) else {
            return Outcome::Stay;
        };
        let start = symbol.jump;
        let (view, doc) = current!(cx.editor);
        // The file changed under the list and the render has not caught up yet.
        if self.read_from.map(|(id, _)| id) != Some(doc.id()) {
            return Outcome::Stay;
        }
        let start = start.min(doc.text().len_chars());
        push_jump(view, doc);
        doc.set_selection(view.id, Selection::point(start));
        align_view(doc, view, Align::Center);
        Outcome::Leave
    }
}

/// Draws a definition on one line: its name, indented by what it sits inside, and what
/// kind of thing it is at the right edge when there is room.
pub fn draw_symbol(surface: &mut Surface, paint: &RowPaint, row: &SymbolRow, theme: &Theme) {
    let mut style = theme.get("ui.text");
    if paint.current {
        style = style.add_modifier(Modifier::BOLD);
    }
    let mut kind_style = if paint.selected.is_some() {
        style
    } else {
        theme.get("ui.text.inactive")
    };
    if let Some(selected) = paint.selected {
        style = style.patch(selected);
        kind_style = kind_style.patch(selected);
    }
    let indent = 3 + row.depth * 2;
    let x = paint.line.x + indent as u16;
    let y = paint.line.y;
    let width = (paint.line.width as usize).saturating_sub(indent + 1);
    let kind_room = row.kind.len() + 1;
    let name_width = if width > kind_room + 8 {
        width - kind_room
    } else {
        width
    };
    let paint_name = |_: usize| -> Style { style };
    surface.set_string_truncated(x, y, &row.name, name_width, paint_name, true, false);
    if name_width < width {
        let kind_x = x + (width - row.kind.len()) as u16;
        surface.set_string(kind_x, y, row.kind, kind_style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symbol(name: &str, kind: &'static str, start: usize, end: usize) -> Symbol {
        Symbol {
            kind,
            name: name.to_string(),
            start,
            end,
            jump: start,
            line: 0,
        }
    }

    fn listed(symbols: &[Symbol]) -> Vec<&Symbol> {
        symbols.iter().collect()
    }

    fn only_callable(symbols: &[Symbol]) -> Vec<&Symbol> {
        symbols
            .iter()
            .filter(|symbol| is_callable(symbol.kind))
            .collect()
    }

    fn names(rows: &[Row]) -> Vec<(String, usize)> {
        rows.iter()
            .map(|row| match row {
                Row::Symbol(symbol) => (symbol.name.clone(), symbol.depth),
                _ => unreachable!(),
            })
            .collect()
    }

    #[test]
    fn definitions_nest_by_containment_in_the_files_order() {
        let symbols = vec![
            symbol("Shape", "class", 0, 100),
            symbol("area", "method", 10, 40),
            symbol("helper", "function", 20, 30),
            symbol("name", "method", 50, 90),
            symbol("main", "function", 120, 150),
        ];
        assert_eq!(
            names(&nested_rows(&listed(&symbols))),
            vec![
                ("Shape".to_string(), 0),
                ("area".to_string(), 1),
                ("helper".to_string(), 2),
                ("name".to_string(), 1),
                ("main".to_string(), 0),
            ]
        );
    }

    #[test]
    fn by_name_is_flat_and_ignores_case() {
        let symbols = vec![
            symbol("zeta", "function", 0, 10),
            symbol("Alpha", "class", 20, 60),
            symbol("beta", "method", 30, 40),
        ];
        assert_eq!(
            names(&named_rows(&listed(&symbols))),
            vec![
                ("Alpha".to_string(), 0),
                ("beta".to_string(), 0),
                ("zeta".to_string(), 0),
            ]
        );
    }

    #[test]
    fn only_functions_leaves_what_is_called_and_nests_it_afresh() {
        let symbols = vec![
            symbol("Shape", "class", 0, 100),
            symbol("width", "field", 5, 8),
            symbol("area", "method", 10, 40),
            symbol("helper", "function", 20, 30),
            symbol("Colour", "enum", 110, 118),
            symbol("main", "function", 120, 150),
        ];
        assert_eq!(
            names(&nested_rows(&only_callable(&symbols))),
            vec![
                ("area".to_string(), 0),
                ("helper".to_string(), 1),
                ("main".to_string(), 0),
            ]
        );
    }

    #[test]
    fn a_saved_layout_survives_a_round_trip() {
        let layout = OutlineLayout {
            shown: true,
            by_name: true,
            only_functions: false,
            beside: true,
            tree_share: 450,
            tree_columns: 400,
        };
        let saved = toml::to_string(&layout).unwrap();
        let restored: OutlineLayout = toml::from_str(&saved).unwrap();
        assert!(restored.shown && restored.by_name && restored.beside);
        assert!(!restored.only_functions);
        let [tree, outline] = stacked_panes(Rect::new(0, 0, 40, 42), restored.tree_share).unwrap();
        assert_eq!(tree.height - 1, 18);
        assert_eq!(outline.bottom(), 42);
        let [tree, outline] = side_panes(Rect::new(0, 0, 42, 20), restored.tree_columns).unwrap();
        assert_eq!(tree.width - 1, 16);
        assert_eq!(outline.right(), 42);
        assert_eq!(tree.right(), outline.x);
    }

    /// A layout written before the outline could sit beside the tree still reads, and the
    /// fields it never held come back at their defaults.
    #[test]
    fn an_older_layout_reads_with_the_new_fields_defaulted() {
        let restored: OutlineLayout =
            toml::from_str("shown = true\nby_name = false\ntree_share = 600\n").unwrap();
        assert!(restored.shown);
        assert!(restored.only_functions);
        assert!(!restored.beside);
        assert_eq!(restored.tree_columns, 500);
    }
}
