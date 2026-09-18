use crate::{
    commands::{self, git::LastBlame, OnKeyCallback, OnKeyCallbackKind},
    compositor::{self, Component, Compositor, Context, Event, EventResult},
    events::{OnModeSwitch, PostCommand},
    handlers::completion::CompletionItem,
    key,
    keymap::{KeymapResult, Keymaps, MappableCommand},
    ui::{
        context_menu,
        document::{render_document, LinePos, TextRenderer},
        lsp::hover::Hover,
        markdown_preview::MarkdownPreview,
        sidebar::{self, Sidebar},
        statusline,
        text_decorations::{self, Decoration, DecorationManager, InlineDiagnostics},
        welcome::Welcome,
        Completion, Popup, ProgressSpinners,
    },
};

use helix_core::{
    diagnostic::NumberOrString,
    graphemes::{next_grapheme_boundary, prev_grapheme_boundary},
    line_ending::line_end_char_index,
    movement::Direction,
    syntax::{self, OverlayHighlights},
    text_annotations::TextAnnotations,
    textobject::{textobject_word, TextObject},
    unicode::width::UnicodeWidthStr,
    visual_offset_from_block, Change, Position, Range, Selection, Transaction,
};
use helix_lsp::lsp;
use helix_view::{
    annotations::diagnostics::DiagnosticFilter,
    document::Mode,
    editor::{CloseError, CompleteAction, CursorShapeConfig},
    graphics::{Color, CursorKind, Margin, Modifier, Rect, Style, UnderlineStyle},
    input::{KeyEvent, MouseButton, MouseEvent, MouseEventKind},
    keyboard::{KeyCode, KeyModifiers},
    tree::Separator,
    Document, DocumentId, Editor, Theme, View,
};
use std::{
    mem::take,
    num::NonZeroUsize,
    ops,
    rc::Rc,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use tui::{buffer::Buffer as Surface, text::Span};

pub struct EditorView {
    pub keymaps: Keymaps,
    welcome: Welcome,
    on_next_key: Option<(OnKeyCallback, OnKeyCallbackKind)>,
    pseudo_pending: Vec<KeyEvent>,
    pub(crate) last_insert: (commands::MappableCommand, Vec<InsertEvent>),
    pub(crate) completion: Option<Completion>,
    spinners: ProgressSpinners,
    /// Tracks if the terminal window is focused by reaction to terminal focus events
    terminal_focused: bool,
    pub(crate) sidebar: Sidebar,
    pub(crate) markdown_preview: MarkdownPreview,
    /// The line blamed last, for a second blame of it to open its commit.
    pub(crate) last_blame: Option<LastBlame>,
    /// The bufferline tabs of the last frame, so a click can land on one.
    bufferline_tabs: Vec<BufferlineTab>,
    /// The rows the bufferline took in the last frame, so the wheel over it can be told.
    bufferline_area: Rect,
    /// The first tab drawn: the strip scrolls when the tabs do not all fit.
    bufferline_first: usize,
    /// The `‹` and `›` marks of the last frame, when tabs were hidden on that side.
    bufferline_back: Option<Rect>,
    bufferline_forward: Option<Rect>,
    /// The split separator being dragged: the mouse is its until the button is let go.
    dragged_separator: Option<Separator>,
    /// The last click in the text, for a second and a third one on the same cell to
    /// take the word and the line.
    last_click: Option<Click>,
    /// What a drag after a double or a triple click extends by, from the unit clicked.
    drag_unit: Option<(ClickUnit, Range)>,
    /// The button went down on the text and is still held: the mouse selects until it is
    /// let go, wherever the pointer goes.
    selecting: bool,
    /// Where the pointer rests.
    pointer: Option<(u16, u16)>,
    /// The pointer shape last asked of the terminal, so it is asked again only on a change.
    pointer_shape: Option<&'static str>,
    /// When it last moved, shared with the hover timer so that it can wait out the moves
    /// without waking the editor: a job landing per cell crossed would be a frame per cell.
    pointer_moved_at: Arc<Mutex<Instant>>,
    /// A hover timer is waiting; a move while it waits pushes it back, never adds one.
    hover_armed: bool,
    /// The document position the hover popup was asked for, not asked again while the
    /// pointer stays on it.
    hover_shown: Option<(DocumentId, usize)>,
}

#[derive(Clone, Copy)]
struct Click {
    at: Instant,
    row: u16,
    column: u16,
    /// One for a click, two for a double click, three for a triple.
    count: u8,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClickUnit {
    Word,
    Line,
}

struct BufferlineTab {
    /// The rows and columns the tab covers, padding included.
    area: Rect,
    /// The column of its close mark.
    close: u16,
    doc: helix_view::DocumentId,
    /// The tab is the file's preview rather than the file itself.
    preview: bool,
}

/// A tab before it is placed: the strip decides which ones fit.
struct TabToDraw {
    name: String,
    mark: &'static str,
    active: bool,
    preview: bool,
    doc: DocumentId,
}

impl TabToDraw {
    fn width(&self) -> u16 {
        self.name.width() as u16 + self.mark.width() as u16 + 2
    }
}

/// The columns a `‹` or `›` mark takes at an edge of the bufferline.
const MARK_WIDTH: u16 = 2;

/// How many tabs from `first` fit in `width` columns, whole, leaving room for the mark
/// on each side that has tabs hidden behind it. Half a tab hides its cross, and the
/// cross is what a click aims for.
fn tabs_that_fit(tabs: &[TabToDraw], first: usize, width: u16) -> usize {
    let mut x = if first > 0 { MARK_WIDTH } else { 0 };
    let mut count = 0;

    for tab in &tabs[first..] {
        let more_after = first + count + 1 < tabs.len();
        let forward_mark = if more_after { MARK_WIDTH } else { 0 };
        if x + tab.width() + forward_mark > width {
            break;
        }

        x += tab.width() + 1;
        count += 1;
    }

    count
}

/// Whether a screen cell is inside an area.
fn hits(area: Rect, row: u16, column: u16) -> bool {
    row >= area.top() && row < area.bottom() && column >= area.left() && column < area.right()
}

/// Where a click on the bufferline landed.
enum BufferlineHit {
    Open(helix_view::DocumentId),
    Close(helix_view::DocumentId),
    ClosePreview,
}

/// The bufferline's rows: a half-row of padding over the names and one under them.
/// The rows the tab strip takes: the names, and under them the half row the tab in front
/// runs on into the editor through. A theme that gives the tab in front no colour of its
/// own, as the 16-colour ones do, has no shape to draw, and the strip is the names alone.
fn bufferline_height(theme: &Theme) -> u16 {
    if bufferline_shaped(theme) {
        2
    } else {
        1
    }
}

/// Whether the tab in front has a colour the bar does not, over an editor with a colour of
/// its own: what drawing it as a tab takes.
fn bufferline_shaped(theme: &Theme) -> bool {
    let background = theme
        .try_get("ui.bufferline.background")
        .unwrap_or_else(|| theme.get("ui.statusline"))
        .bg;
    let active = theme
        .try_get("ui.bufferline.active")
        .unwrap_or_else(|| theme.get("ui.statusline.active"))
        .bg;
    active.is_some() && active != background && theme.get("ui.background").bg.is_some()
}

/// How many lines a click has to land away from the cursor to count as a jump.
const JUMP_LINES: usize = 10;

/// How soon after a click a second one on the same cell counts as a double click.
const DOUBLE_CLICK: Duration = Duration::from_millis(500);

/// How long the pointer rests on the text before what is under it is asked for.
const HOVER_DELAY: Duration = Duration::from_millis(400);

#[derive(Debug, Clone)]
pub enum InsertEvent {
    Key(KeyEvent),
    CompletionApply {
        trigger_offset: usize,
        changes: Vec<Change>,
    },
    TriggerCompletion,
    RequestCompletion,
}

impl EditorView {
    pub fn new(keymaps: Keymaps, sidebar: Sidebar) -> Self {
        Self {
            keymaps,
            on_next_key: None,
            pseudo_pending: Vec::new(),
            last_insert: (commands::MappableCommand::normal_mode, Vec::new()),
            completion: None,
            spinners: ProgressSpinners::default(),
            terminal_focused: true,
            sidebar,
            markdown_preview: MarkdownPreview::new(),
            last_blame: None,
            bufferline_tabs: Vec::new(),
            bufferline_area: Rect::default(),
            bufferline_first: 0,
            bufferline_back: None,
            bufferline_forward: None,
            dragged_separator: None,
            last_click: None,
            drag_unit: None,
            selecting: false,
            pointer: None,
            pointer_shape: None,
            pointer_moved_at: Arc::new(Mutex::new(Instant::now())),
            hover_armed: false,
            hover_shown: None,
            welcome: Welcome::default(),
        }
    }

    pub fn spinners_mut(&mut self) -> &mut ProgressSpinners {
        &mut self.spinners
    }

    pub fn render_view(
        &self,
        editor: &Editor,
        doc: &Document,
        view: &View,
        viewport: Rect,
        surface: &mut Surface,
        is_focused: bool,
    ) {
        let inner = view.inner_area(doc);
        let area = view.area;
        let theme = &editor.theme;
        let config = editor.config();
        let loader = editor.syn_loader.load();

        let view_offset = doc.view_offset(view.id);

        let text_annotations = view.text_annotations(doc, Some(theme));
        let mut decorations = DecorationManager::default();

        if is_focused && config.cursorline {
            decorations.add_decoration(Self::cursorline(doc, view, theme));
        }

        if is_focused && config.cursorcolumn {
            Self::highlight_cursorcolumn(doc, view, surface, theme, inner, &text_annotations);
        }

        // Set DAP highlights, if needed.
        if let Some(frame) = editor.current_stack_frame() {
            let dap_line = frame.line.saturating_sub(1);
            let style = theme.get("ui.highlight.frameline");
            let line_decoration = move |renderer: &mut TextRenderer, pos: LinePos| {
                if pos.doc_line != dap_line {
                    return;
                }
                renderer.set_style(Rect::new(inner.x, pos.visual_line, inner.width, 1), style);
            };

            decorations.add_decoration(line_decoration);
        }

        let syntax_highlighter =
            Self::doc_syntax_highlighter(doc, view_offset.anchor, inner.height, &loader);
        let mut overlays = Vec::new();
        if let Some(review) = &doc.review {
            let text = doc.text();
            let first = text.char_to_line(view_offset.anchor.min(text.len_chars()));
            let end = (first + inner.height as usize + 1).min(text.len_lines());
            overlays.extend(review.highlights(text, first..end, &loader));
        }

        overlays.push(Self::overlay_syntax_highlights(
            doc,
            view_offset.anchor,
            inner.height,
            &text_annotations,
        ));

        if doc
            .language_config()
            .and_then(|config| config.rainbow_brackets)
            .unwrap_or(config.rainbow_brackets)
        {
            if let Some(overlay) =
                Self::doc_rainbow_highlights(doc, view_offset.anchor, inner.height, theme, &loader)
            {
                overlays.push(overlay);
            }
        }

        if let Some(overlay) = Self::doc_document_link_highlights(doc, theme) {
            overlays.push(overlay);
        }

        Self::doc_diagnostics_highlights_into(doc, theme, &mut overlays);

        if is_focused {
            if config.lsp.auto_document_highlight {
                if let Some(overlay) = Self::doc_document_highlights(doc, view, theme) {
                    overlays.push(overlay);
                }
            }
            if let Some(tabstops) = Self::tabstop_highlights(doc, theme) {
                overlays.push(tabstops);
            }
            overlays.push(Self::doc_selection_highlights(
                editor.mode(),
                doc,
                view,
                theme,
                &config.cursor_shape,
                // A blinking block is the terminal's own cursor, since only the terminal
                // blinks; a steady one is drawn here, in the theme's colours.
                self.terminal_focused && !config.cursor_blink,
            ));
            if let Some(overlay) = Self::highlight_focused_view_elements(view, doc, theme) {
                overlays.push(overlay);
            }
        }

        let gutter_overflow = view.gutter_offset(doc) == 0;
        if !gutter_overflow && doc.review.is_none() {
            Self::render_gutter(
                editor,
                doc,
                view,
                view.area,
                theme,
                is_focused & self.terminal_focused,
                &mut decorations,
            );
        }

        Self::render_rulers(editor, doc, view, inner, surface, theme);

        let primary_cursor = doc
            .selection(view.id)
            .primary()
            .cursor(doc.text().slice(..));
        if is_focused {
            decorations.add_decoration(text_decorations::Cursor {
                cache: &editor.cursor_cache,
                primary_cursor,
            });
        }
        let width = view.inner_width(doc);
        let config = doc.config.load();
        let enable_cursor_line = view
            .diagnostics_handler
            .show_cursorline_diagnostics(doc, view.id);
        let inline_diagnostic_config = config.inline_diagnostics.prepare(width, enable_cursor_line);
        decorations.add_decoration(InlineDiagnostics::new(
            doc,
            theme,
            primary_cursor,
            inline_diagnostic_config,
            config.end_of_line_diagnostics,
        ));
        render_document(
            surface,
            inner,
            doc,
            view_offset,
            &text_annotations,
            syntax_highlighter,
            overlays,
            theme,
            decorations,
        );

        // if we're not at the edge of the screen, draw a right border
        if viewport.right() != view.area.right() {
            let x = area.right();
            let border_style = theme.get("ui.window");
            for y in area.top()..area.bottom() {
                surface[(x, y)]
                    .set_symbol(tui::symbols::line::VERTICAL)
                    //.set_symbol(" ")
                    .set_style(border_style);
            }
        }

        // The box waits for the caret to settle, as the message written between the lines
        // used to: it does not flash past while the caret crosses the line.
        if config.inline_diagnostics.disabled()
            && config.end_of_line_diagnostics == DiagnosticFilter::Disable
            && enable_cursor_line
        {
            Self::render_diagnostics(doc, view, inner, surface, theme);
        }

        let statusline_area = view
            .area
            .clip_top(view.area.height.saturating_sub(1))
            .clip_bottom(1); // -1 from bottom to remove commandline

        let mut context =
            statusline::RenderContext::new(editor, doc, view, is_focused, &self.spinners);

        statusline::render(&mut context, statusline_area, surface);
    }

    pub fn render_rulers(
        editor: &Editor,
        doc: &Document,
        view: &View,
        viewport: Rect,
        surface: &mut Surface,
        theme: &Theme,
    ) {
        let editor_rulers = &editor.config().rulers;
        let ruler_theme = theme
            .try_get("ui.virtual.ruler")
            .unwrap_or_else(|| Style::default().bg(Color::Red));

        let rulers = doc
            .language_config()
            .and_then(|config| config.rulers.as_ref())
            .unwrap_or(editor_rulers);

        let view_offset = doc.view_offset(view.id);

        rulers
            .iter()
            // View might be horizontally scrolled, convert from absolute distance
            // from the 1st column to relative distance from left of viewport
            .filter_map(|ruler| ruler.checked_sub(1 + view_offset.horizontal_offset as u16))
            .filter(|ruler| ruler < &viewport.width)
            .map(|ruler| viewport.clip_left(ruler).with_width(1))
            .for_each(|area| surface.set_style(area, ruler_theme))
    }

    fn viewport_byte_range(
        text: helix_core::RopeSlice,
        row: usize,
        height: u16,
    ) -> std::ops::Range<usize> {
        // Calculate viewport byte ranges:
        // Saturating subs to make it inclusive zero indexing.
        let last_line = text.len_lines().saturating_sub(1);
        let last_visible_line = (row + height as usize).saturating_sub(1).min(last_line);
        let start = text.line_to_byte(row.min(last_line));
        let end = text.line_to_byte(last_visible_line + 1);

        start..end
    }

    /// Get the syntax highlighter for a document in a view represented by the first line
    /// and column (`offset`) and the last line. This is done instead of using a view
    /// directly to enable rendering syntax highlighted docs anywhere (eg. picker preview)
    pub fn doc_syntax_highlighter<'editor>(
        doc: &'editor Document,
        anchor: usize,
        height: u16,
        loader: &'editor syntax::Loader,
    ) -> Option<syntax::Highlighter<'editor>> {
        let syntax = doc.syntax()?;
        let text = doc.text().slice(..);
        let row = text.char_to_line(anchor.min(text.len_chars()));
        let range = Self::viewport_byte_range(text, row, height);
        let range = range.start as u32..range.end as u32;

        let highlighter = syntax.highlighter(text, loader, range);
        Some(highlighter)
    }

    pub fn overlay_syntax_highlights(
        doc: &Document,
        anchor: usize,
        height: u16,
        text_annotations: &TextAnnotations,
    ) -> OverlayHighlights {
        let text = doc.text().slice(..);
        let row = text.char_to_line(anchor.min(text.len_chars()));

        let mut range = Self::viewport_byte_range(text, row, height);
        range = text.byte_to_char(range.start)..text.byte_to_char(range.end);

        text_annotations.collect_overlay_highlights(range)
    }

    pub fn doc_rainbow_highlights(
        doc: &Document,
        anchor: usize,
        height: u16,
        theme: &Theme,
        loader: &syntax::Loader,
    ) -> Option<OverlayHighlights> {
        let syntax = doc.syntax()?;
        let text = doc.text().slice(..);
        let row = text.char_to_line(anchor.min(text.len_chars()));
        let visible_range = Self::viewport_byte_range(text, row, height);
        let start = syntax::child_for_byte_range(
            &syntax.tree().root_node(),
            visible_range.start as u32..visible_range.end as u32,
        )
        .map_or(visible_range.start as u32, |node| node.start_byte());
        let range = start..visible_range.end as u32;

        Some(syntax.rainbow_highlights(text, theme.rainbow_length(), loader, range))
    }

    /// Get highlight spans for document diagnostics
    pub fn doc_diagnostics_highlights_into(
        doc: &Document,
        theme: &Theme,
        overlay_highlights: &mut Vec<OverlayHighlights>,
    ) {
        // Skip redundant work if no diagnostics.
        if doc.diagnostics().is_empty() {
            return;
        }

        use helix_core::diagnostic::{DiagnosticTag, Range, Severity};
        let get_scope_of = |scope| {
            theme
                .find_highlight_exact(scope)
                // get one of the themes below as fallback values
                .or_else(|| theme.find_highlight_exact("diagnostic"))
                .or_else(|| theme.find_highlight_exact("ui.cursor"))
                .or_else(|| theme.find_highlight_exact("ui.selection"))
                .expect(
                    "at least one of the following scopes must be defined in the theme: `diagnostic`, `ui.cursor`, or `ui.selection`",
                )
        };

        // Diagnostic tags
        let unnecessary = theme.find_highlight_exact("diagnostic.unnecessary");
        let deprecated = theme.find_highlight_exact("diagnostic.deprecated");

        let mut default_vec = Vec::new();
        let mut info_vec = Vec::new();
        let mut hint_vec = Vec::new();
        let mut warning_vec = Vec::new();
        let mut error_vec = Vec::new();
        let mut unnecessary_vec = Vec::new();
        let mut deprecated_vec = Vec::new();

        let push_diagnostic = |vec: &mut Vec<ops::Range<usize>>, range: Range| {
            // If any diagnostic overlaps ranges with the prior diagnostic,
            // merge the two together. Otherwise push a new span.
            match vec.last_mut() {
                Some(existing_range) if range.start <= existing_range.end => {
                    // This branch merges overlapping diagnostics, assuming that the current
                    // diagnostic starts on range.start or later. If this assertion fails,
                    // we will discard some part of `diagnostic`. This implies that
                    // `doc.diagnostics()` is not sorted by `diagnostic.range`.
                    debug_assert!(existing_range.start <= range.start);
                    existing_range.end = range.end.max(existing_range.end)
                }
                _ => vec.push(range.start..range.end),
            }
        };

        for diagnostic in doc.diagnostics() {
            // Separate diagnostics into different Vecs by severity.
            let vec = match diagnostic.severity {
                Some(Severity::Info) => &mut info_vec,
                Some(Severity::Hint) => &mut hint_vec,
                Some(Severity::Warning) => &mut warning_vec,
                Some(Severity::Error) => &mut error_vec,
                _ => &mut default_vec,
            };

            // If the diagnostic has tags and a non-warning/error severity, skip rendering
            // the diagnostic as info/hint/default and only render it as unnecessary/deprecated
            // instead. For warning/error diagnostics, render both the severity highlight and
            // the tag highlight.
            if diagnostic.tags.is_empty()
                || matches!(
                    diagnostic.severity,
                    Some(Severity::Warning | Severity::Error)
                )
            {
                push_diagnostic(vec, diagnostic.range);
            }

            for tag in &diagnostic.tags {
                match tag {
                    DiagnosticTag::Unnecessary => {
                        if unnecessary.is_some() {
                            push_diagnostic(&mut unnecessary_vec, diagnostic.range)
                        }
                    }
                    DiagnosticTag::Deprecated => {
                        if deprecated.is_some() {
                            push_diagnostic(&mut deprecated_vec, diagnostic.range)
                        }
                    }
                }
            }
        }

        overlay_highlights.push(OverlayHighlights::Homogeneous {
            highlight: get_scope_of("diagnostic"),
            ranges: default_vec,
        });
        if let Some(highlight) = unnecessary {
            overlay_highlights.push(OverlayHighlights::Homogeneous {
                highlight,
                ranges: unnecessary_vec,
            });
        }
        if let Some(highlight) = deprecated {
            overlay_highlights.push(OverlayHighlights::Homogeneous {
                highlight,
                ranges: deprecated_vec,
            });
        }
        overlay_highlights.extend([
            OverlayHighlights::Homogeneous {
                highlight: get_scope_of("diagnostic.info"),
                ranges: info_vec,
            },
            OverlayHighlights::Homogeneous {
                highlight: get_scope_of("diagnostic.hint"),
                ranges: hint_vec,
            },
            OverlayHighlights::Homogeneous {
                highlight: get_scope_of("diagnostic.warning"),
                ranges: warning_vec,
            },
            OverlayHighlights::Homogeneous {
                highlight: get_scope_of("diagnostic.error"),
                ranges: error_vec,
            },
        ]);
    }

    pub fn doc_document_highlights(
        doc: &Document,
        view: &View,
        theme: &Theme,
    ) -> Option<OverlayHighlights> {
        let ranges = doc.document_highlights(view.id)?;
        if ranges.is_empty() {
            return None;
        }

        let highlight = theme
            .find_highlight_exact("ui.highlight")
            .or_else(|| theme.find_highlight_exact("ui.selection"))
            .or_else(|| theme.find_highlight_exact("ui.cursor"))?;

        Some(OverlayHighlights::Homogeneous {
            highlight,
            ranges: ranges.to_vec(),
        })
    }

    pub fn doc_document_link_highlights(
        doc: &Document,
        theme: &Theme,
    ) -> Option<OverlayHighlights> {
        let highlight = theme
            .find_highlight_exact("markup.link.url")
            .or_else(|| theme.find_highlight_exact("markup.link"))?;

        if doc.document_links.is_empty() {
            return None;
        }

        let mut ranges: Vec<ops::Range<usize>> = Vec::new();
        for link in &doc.document_links {
            if link.start >= link.end {
                continue;
            }

            match ranges.last_mut() {
                Some(existing_range) if link.start <= existing_range.end => {
                    existing_range.end = existing_range.end.max(link.end);
                }
                _ => ranges.push(link.start..link.end),
            }
        }

        if ranges.is_empty() {
            return None;
        }

        Some(OverlayHighlights::Homogeneous { highlight, ranges })
    }

    /// Get highlight spans for selections in a document view.
    pub fn doc_selection_highlights(
        mode: Mode,
        doc: &Document,
        view: &View,
        theme: &Theme,
        cursor_shape_config: &CursorShapeConfig,
        is_terminal_focused: bool,
    ) -> OverlayHighlights {
        let text = doc.text().slice(..);
        let selection = doc.selection(view.id);
        let primary_idx = selection.primary_index();

        let cursorkind = cursor_shape_config.from_mode(mode);
        let cursor_is_block = cursorkind == CursorKind::Block;

        let selection_scope = theme
            .find_highlight_exact("ui.selection")
            .expect("could not find `ui.selection` scope in the theme!");
        let primary_selection_scope = theme
            .find_highlight_exact("ui.selection.primary")
            .unwrap_or(selection_scope);

        let base_cursor_scope = theme
            .find_highlight_exact("ui.cursor")
            .unwrap_or(selection_scope);
        let base_primary_cursor_scope = theme
            .find_highlight("ui.cursor.primary")
            .unwrap_or(base_cursor_scope);

        let cursor_scope = match mode {
            Mode::Insert => theme.find_highlight_exact("ui.cursor.insert"),
            Mode::Select => theme.find_highlight_exact("ui.cursor.select"),
            Mode::Normal => theme.find_highlight_exact("ui.cursor.normal"),
        }
        .unwrap_or(base_cursor_scope);

        let primary_cursor_scope = match mode {
            Mode::Insert => theme.find_highlight_exact("ui.cursor.primary.insert"),
            Mode::Select => theme.find_highlight_exact("ui.cursor.primary.select"),
            Mode::Normal => theme.find_highlight_exact("ui.cursor.primary.normal"),
        }
        .unwrap_or(base_primary_cursor_scope);

        let mut spans = Vec::new();
        for (i, range) in selection.iter().enumerate() {
            let selection_is_primary = i == primary_idx;
            let (cursor_scope, selection_scope) = if selection_is_primary {
                (primary_cursor_scope, primary_selection_scope)
            } else {
                (cursor_scope, selection_scope)
            };

            // Special-case: cursor at end of the rope.
            if range.head == range.anchor && range.head == text.len_chars() {
                if !selection_is_primary || (cursor_is_block && is_terminal_focused) {
                    // Bar and underline cursors are drawn by the terminal
                    // BUG: If the editor area loses focus while having a bar or
                    // underline cursor (eg. when a regex prompt has focus) then
                    // the primary cursor will be invisible. This doesn't happen
                    // with block cursors since we manually draw *all* cursors.
                    spans.push((cursor_scope, range.head..range.head + 1));
                }
                continue;
            }

            let range = range.min_width_1(text);
            if range.head > range.anchor {
                // Standard case.
                let cursor_start = prev_grapheme_boundary(text, range.head);
                // Explicit insert selections replace the entire range when typing,
                // including the grapheme beneath the terminal's bar/underline cursor.
                let selection_end = if selection_is_primary
                    && !cursor_is_block
                    && (mode != Mode::Insert || view.insert_selection)
                {
                    range.head
                } else {
                    cursor_start
                };
                spans.push((selection_scope, range.anchor..selection_end));
                // add block cursors
                // skip primary cursor if terminal is unfocused - terminal cursor is used in that case
                if !selection_is_primary || (cursor_is_block && is_terminal_focused) {
                    spans.push((cursor_scope, cursor_start..range.head));
                }
            } else {
                // Reverse case.
                let cursor_end = next_grapheme_boundary(text, range.head);
                // add block cursors
                // skip primary cursor if terminal is unfocused - terminal cursor is used in that case
                if !selection_is_primary || (cursor_is_block && is_terminal_focused) {
                    spans.push((cursor_scope, range.head..cursor_end));
                }
                // non block cursors look like they exclude the cursor
                let selection_start = if selection_is_primary
                    && !cursor_is_block
                    && !(mode == Mode::Insert
                        && !view.insert_selection
                        && cursor_end == range.anchor)
                {
                    range.head
                } else {
                    cursor_end
                };
                spans.push((selection_scope, selection_start..range.anchor));
            }
        }

        OverlayHighlights::Heterogenous { highlights: spans }
    }

    /// Render brace match, etc (meant for the focused view only)
    pub fn highlight_focused_view_elements(
        view: &View,
        doc: &Document,
        theme: &Theme,
    ) -> Option<OverlayHighlights> {
        // Highlight matching braces
        let syntax = doc.syntax()?;
        let highlight = theme.find_highlight_exact("ui.cursor.match")?;
        let text = doc.text().slice(..);
        let pos = doc.selection(view.id).primary().cursor(text);
        let pos = helix_core::match_brackets::find_matching_bracket(syntax, text, pos)?;
        Some(OverlayHighlights::single(highlight, pos..pos + 1))
    }

    pub fn tabstop_highlights(doc: &Document, theme: &Theme) -> Option<OverlayHighlights> {
        let snippet = doc.active_snippet.as_ref()?;
        let highlight = theme.find_highlight_exact("tabstop")?;
        let mut ranges = Vec::new();
        for tabstop in snippet.tabstops() {
            ranges.extend(tabstop.ranges.iter().map(|range| range.start..range.end));
        }
        Some(OverlayHighlights::Homogeneous { highlight, ranges })
    }

    /// Render bufferline at the top
    pub fn render_bufferline(&mut self, editor: &Editor, viewport: Rect, surface: &mut Surface) {
        self.clear_bufferline();
        self.bufferline_area = viewport;
        surface.clear_with(
            viewport,
            editor
                .theme
                .try_get("ui.bufferline.background")
                .unwrap_or_else(|| editor.theme.get("ui.statusline")),
        );

        // The tab in front is told apart by its shape and its colour. The underline the
        // status line wears comes along when a theme has no tab colours of its own, and
        // under a title it reads as a stray rule.
        let shaped = bufferline_shaped(&editor.theme);
        let height = bufferline_height(&editor.theme);
        let bufferline_active = editor
            .theme
            .try_get("ui.bufferline.active")
            .unwrap_or_else(|| editor.theme.get("ui.statusline.active"))
            .underline_style(UnderlineStyle::Reset);
        // Without a colour of its own, the tab in front is cut out of the bar, in bold.
        let bufferline_active = if shaped {
            bufferline_active
        } else {
            bufferline_active
                .bg(Color::Reset)
                .add_modifier(Modifier::BOLD)
        };

        let bufferline_inactive = editor
            .theme
            .try_get("ui.bufferline")
            .unwrap_or_else(|| editor.theme.get("ui.statusline.inactive"));

        let background = editor
            .theme
            .try_get("ui.bufferline.background")
            .unwrap_or_else(|| editor.theme.get("ui.statusline"))
            .bg;
        let editor_background = editor.theme.get("ui.background").bg;

        // The names on the first row, and under them the bar ends half-way down, where the tab
        // in front runs on into the editor.
        let top = viewport.y;
        let middle = top;
        let bottom = top + 1;

        // The bar ends half-way down its last row, so it sits on the editor.
        if shaped {
            for x in viewport.left()..viewport.right() {
                draw_half_block(surface, x, bottom, background, editor_background);
            }
        }

        let current_doc = view!(editor).doc;

        // The preview filling the screen is a tab of its own beside its file's, and it is
        // the one lit: the file it renders is not what you are looking at.
        let full_preview = self.markdown_preview.is_full(editor);

        let mut tabs = Vec::new();
        for doc in editor.documents() {
            let fname = match doc.path() {
                Some(path) => path
                    .file_name()
                    .unwrap_or_default()
                    .to_str()
                    .unwrap_or_default(),
                None => doc.scratch_name(),
            };

            let is_current = current_doc == doc.id();

            // A modified buffer shows a dot where the cross goes, as it cannot be
            // closed without losing its changes.
            let mark = if doc.is_modified() { "●" } else { "×" };
            tabs.push(TabToDraw {
                name: format!("  {fname}  "),
                mark,
                active: is_current && !full_preview,
                preview: false,
                doc: doc.id(),
            });
            if full_preview && is_current {
                tabs.push(TabToDraw {
                    name: format!("  {fname} ✓  "),
                    mark: "×",
                    active: true,
                    preview: true,
                    doc: doc.id(),
                });
            }
        }

        // The strip starts where it was left, and moves only as far as it must for the
        // tab in front to be on screen.
        let current = tabs.iter().position(|tab| tab.active).unwrap_or(0);
        let mut first = self.bufferline_first.min(current);
        let mut shown = tabs_that_fit(&tabs, first, viewport.width);
        while first < current && first + shown <= current {
            first += 1;
            shown = tabs_that_fit(&tabs, first, viewport.width);
        }
        self.bufferline_first = first;

        let mut x = viewport.x;
        if first > 0 {
            let area = Rect::new(x, top, MARK_WIDTH, height);
            surface.set_string(x, middle, "‹", bufferline_inactive);
            self.bufferline_back = Some(area);
            x += MARK_WIDTH;
        }

        for tab in &tabs[first..first + shown] {
            let style = if tab.active {
                bufferline_active
            } else {
                bufferline_inactive
            };
            let tab_background = style.bg.or(background);
            let width = tab.width();

            let area = Rect::new(x, top, width, height);
            for column in area.left()..area.right() {
                if shaped {
                    draw_half_block(surface, column, bottom, tab_background, editor_background);
                }
            }

            let close = x + tab.name.width() as u16;
            surface.set_string(x, middle, &tab.name, style);
            surface.set_string(close, middle, tab.mark, style);
            surface.set_string(close + tab.mark.width() as u16, middle, "  ", style);

            self.bufferline_tabs.push(BufferlineTab {
                area,
                close,
                doc: tab.doc,
                preview: tab.preview,
            });

            // One column of bar between tabs.
            x += width + 1;
        }

        if first + shown < tabs.len() {
            let x = viewport.right() - MARK_WIDTH;
            let area = Rect::new(x, top, MARK_WIDTH, height);
            surface.set_string(x + 1, middle, "›", bufferline_inactive);
            self.bufferline_forward = Some(area);
        }
    }

    /// Forgets the last frame's bufferline, so nothing of it can be clicked.
    fn clear_bufferline(&mut self) {
        self.bufferline_tabs.clear();
        self.bufferline_area = Rect::default();
        self.bufferline_back = None;
        self.bufferline_forward = None;
    }

    fn bufferline_hit(&self, row: u16, column: u16) -> Option<BufferlineHit> {
        let tab = self
            .bufferline_tabs
            .iter()
            .find(|tab| hits(tab.area, row, column))?;

        // The cross takes a column either side too: a single cell is a small target.
        let closing = column + 1 >= tab.close && column <= tab.close + 1;

        match (tab.preview, closing) {
            (true, true) => Some(BufferlineHit::ClosePreview),
            (true, false) => None,
            (false, true) => Some(BufferlineHit::Close(tab.doc)),
            (false, false) => Some(BufferlineHit::Open(tab.doc)),
        }
    }

    pub fn render_gutter<'d>(
        editor: &'d Editor,
        doc: &'d Document,
        view: &View,
        viewport: Rect,
        theme: &Theme,
        is_focused: bool,
        decoration_manager: &mut DecorationManager<'d>,
    ) {
        let text = doc.text().slice(..);
        let cursors: Rc<[_]> = doc
            .selection(view.id)
            .iter()
            .map(|range| range.cursor_line(text))
            .collect();

        let mut offset = 0;

        let gutter_style = theme.get("ui.gutter");
        let gutter_selected_style = theme.get("ui.gutter.selected");
        let gutter_style_virtual = theme.get("ui.gutter.virtual");
        let gutter_selected_style_virtual = theme.get("ui.gutter.selected.virtual");

        for gutter_type in view.gutters() {
            let mut gutter = gutter_type.style(editor, doc, view, theme, is_focused);
            let width = gutter_type.width(view, doc);
            // avoid lots of small allocations by reusing a text buffer for each line
            let mut text = String::with_capacity(width);
            let cursors = cursors.clone();
            let gutter_decoration = move |renderer: &mut TextRenderer, pos: LinePos| {
                // TODO handle softwrap in gutters
                let selected = cursors.contains(&pos.doc_line);
                let x = viewport.x + offset;
                let y = pos.visual_line;

                let gutter_style = match (selected, pos.first_visual_line) {
                    (false, true) => gutter_style,
                    (true, true) => gutter_selected_style,
                    (false, false) => gutter_style_virtual,
                    (true, false) => gutter_selected_style_virtual,
                };

                if let Some(style) =
                    gutter(pos.doc_line, selected, pos.first_visual_line, &mut text)
                {
                    renderer.set_stringn(x, y, &text, width, gutter_style.patch(style));
                } else {
                    renderer.set_style(
                        Rect {
                            x,
                            y,
                            width: width as u16,
                            height: 1,
                        },
                        gutter_style,
                    );
                }
                text.clear();
            };
            decoration_manager.add_decoration(gutter_decoration);

            offset += width as u16;
        }
    }

    /// What is wrong with the line the caret is on, in a box floating over the code under
    /// it — over the code above it when there is no room below. It is drawn ON the text
    /// and never inside it: nothing in the file moves because the caret landed on a
    /// mistake, and what was under the pointer is one keystroke or one move away again.
    pub fn render_diagnostics(
        doc: &Document,
        view: &View,
        viewport: Rect,
        surface: &mut Surface,
        theme: &Theme,
    ) {
        use helix_core::diagnostic::Severity;
        use tui::{
            text::Text,
            widgets::{Block, Paragraph, Widget},
        };

        if viewport.width < 16 || viewport.height < 4 {
            return;
        }

        let text = doc.text().slice(..);
        let cursor = doc.selection(view.id).primary().cursor(text);
        let cursor_line = text.char_to_line(cursor);

        // Only what is worth interrupting the reading of the file for: a hint is told by
        // the underline under it and by resting the pointer on it.
        let diagnostics: Vec<_> = doc
            .diagnostics()
            .iter()
            .filter(|diagnostic| {
                diagnostic.line == cursor_line
                    && matches!(
                        diagnostic.severity,
                        Some(Severity::Error) | Some(Severity::Warning) | None
                    )
            })
            .collect();
        if diagnostics.is_empty() {
            return;
        }

        let Some(position) = view.screen_coords_at_pos(doc, text, cursor) else {
            return;
        };

        let max_width = 72.min(viewport.width.saturating_sub(2)) as usize;
        // The box is the text plus a space and a border on each side.
        let content_width = max_width.saturating_sub(4);
        let mut lines: Vec<Span> = Vec::new();
        for diagnostic in diagnostics {
            let style = theme.get(match diagnostic.severity {
                Some(Severity::Error) => "error",
                Some(Severity::Warning) | None => "warning",
                Some(Severity::Info) => "info",
                Some(Severity::Hint) => "hint",
            });
            let code = match diagnostic.code.as_ref() {
                Some(NumberOrString::Number(n)) => format!(" ({n})"),
                Some(NumberOrString::String(s)) => format!(" ({s})"),
                None => String::new(),
            };
            let message = format!("{}{code}", diagnostic.message.replace('\n', " "));
            for line in wrap_to_width(&message, content_width) {
                lines.push(Span::styled(line, style));
            }
        }

        // Under the caret when it fits there, over it when it does not, and on the side
        // with the most room when neither holds it whole. Eight rows at most: past that
        // the box hides more of the file than the message is worth, and all of it is one
        // rest of the pointer away.
        let caret_row = viewport.y + position.row as u16;
        let under = viewport.bottom().saturating_sub(caret_row + 1);
        let over = caret_row.saturating_sub(viewport.y);
        let room = under.max(over);
        // Under three rows there is no box to draw: a border, a line and a border.
        if room < 3 {
            return;
        }

        let height = ((lines.len().min(8) + 2) as u16).min(room);
        let rows = height as usize - 2;
        if lines.len() > rows {
            lines.truncate(rows);
            // What did not fit is still one rest of the pointer away.
            if let Some(last) = lines.last_mut() {
                let style = last.style;
                *last = Span::styled(format!("{}…", last.content), style);
            }
        }
        let width = lines
            .iter()
            .map(|line| line.content.width())
            .max()
            .unwrap_or(0) as u16
            + 4;

        // The line the caret is on is never covered, whichever way the box goes.
        let y = if height <= under {
            caret_row + 1
        } else {
            caret_row - height
        };
        let x = (viewport.x + position.col as u16).min(viewport.right().saturating_sub(width));
        let area = viewport.intersection(Rect::new(x, y, width, height));

        let popup_style = theme.get("ui.popup");
        surface.clear_with(area, popup_style);
        let block = Block::bordered().border_style(popup_style);
        let inner = block.inner(area).inner(Margin::horizontal(1));
        block.render(area, surface);
        Paragraph::new(&Text::from(
            lines
                .into_iter()
                .map(tui::text::Spans::from)
                .collect::<Vec<_>>(),
        ))
        .render(inner, surface);
    }

    /// Apply the highlighting on the lines where a cursor is active
    pub fn cursorline(doc: &Document, view: &View, theme: &Theme) -> impl Decoration {
        let text = doc.text().slice(..);
        // TODO only highlight the visual line that contains the cursor instead of the full visual line
        let primary_line = doc.selection(view.id).primary().cursor_line(text);

        // The secondary_lines do contain the primary_line, it doesn't matter
        // as the else-if clause in the loop later won't test for the
        // secondary_lines if primary_line == line.
        // It's used inside a loop so the collect isn't needless:
        // https://github.com/rust-lang/rust-clippy/issues/6164
        #[allow(clippy::needless_collect)]
        let secondary_lines: Vec<_> = doc
            .selection(view.id)
            .iter()
            .map(|range| range.cursor_line(text))
            .collect();

        let primary_style = theme.get("ui.cursorline.primary");
        let secondary_style = theme.get("ui.cursorline.secondary");
        let viewport = view.area;

        move |renderer: &mut TextRenderer, pos: LinePos| {
            let area = Rect::new(viewport.x, pos.visual_line, viewport.width, 1);
            if primary_line == pos.doc_line {
                renderer.set_style(area, primary_style);
            } else if secondary_lines.binary_search(&pos.doc_line).is_ok() {
                renderer.set_style(area, secondary_style);
            }
        }
    }

    /// Apply the highlighting on the columns where a cursor is active
    pub fn highlight_cursorcolumn(
        doc: &Document,
        view: &View,
        surface: &mut Surface,
        theme: &Theme,
        viewport: Rect,
        text_annotations: &TextAnnotations,
    ) {
        let text = doc.text().slice(..);

        // Manual fallback behaviour:
        // ui.cursorcolumn.{p/s} -> ui.cursorcolumn -> ui.cursorline.{p/s}
        let primary_style = theme
            .try_get_exact("ui.cursorcolumn.primary")
            .or_else(|| theme.try_get_exact("ui.cursorcolumn"))
            .unwrap_or_else(|| theme.get("ui.cursorline.primary"));
        let secondary_style = theme
            .try_get_exact("ui.cursorcolumn.secondary")
            .or_else(|| theme.try_get_exact("ui.cursorcolumn"))
            .unwrap_or_else(|| theme.get("ui.cursorline.secondary"));

        let inner_area = view.inner_area(doc);

        let selection = doc.selection(view.id);
        let view_offset = doc.view_offset(view.id);
        let primary = selection.primary();
        let text_format = doc.text_format(viewport.width, None);
        for range in selection.iter() {
            let is_primary = primary == *range;
            let cursor = range.cursor(text);

            let Position { col, .. } =
                visual_offset_from_block(text, cursor, cursor, &text_format, text_annotations).0;

            // if the cursor is horizontally in the view
            if col >= view_offset.horizontal_offset
                && inner_area.width > (col - view_offset.horizontal_offset) as u16
            {
                let area = Rect::new(
                    inner_area.x + (col - view_offset.horizontal_offset) as u16,
                    view.area.y,
                    1,
                    view.area.height,
                );
                if is_primary {
                    surface.set_style(area, primary_style)
                } else {
                    surface.set_style(area, secondary_style)
                }
            }
        }
    }

    /// Handle events by looking them up in `self.keymaps`. Returns None
    /// if event was handled (a command was executed or a subkeymap was
    /// activated). Only KeymapResult::{NotFound, Cancelled} is returned
    /// otherwise.
    fn handle_keymap_event(
        &mut self,
        mode: Mode,
        cxt: &mut commands::Context,
        event: KeyEvent,
    ) -> Option<KeymapResult> {
        let mut last_mode = mode;
        self.pseudo_pending.extend(self.keymaps.pending());
        let key_result = self.keymaps.get(mode, event);
        cxt.editor.autoinfo = self.keymaps.sticky().map(|node| node.infobox());

        let mut execute_command = |command: &commands::MappableCommand| {
            command.execute(cxt);
            helix_event::dispatch(PostCommand { command, cx: cxt });

            let current_mode = cxt.editor.mode();
            if current_mode != last_mode {
                helix_event::dispatch(OnModeSwitch {
                    old_mode: last_mode,
                    new_mode: current_mode,
                    cx: cxt,
                });

                // HAXX: if we just entered insert mode from normal, clear key buf
                // and record the command that got us into this mode.
                if current_mode == Mode::Insert {
                    // how we entered insert mode is important, and we should track that so
                    // we can repeat the side effect.
                    self.last_insert.0 = command.clone();
                    self.last_insert.1.clear();
                }
            }

            last_mode = current_mode;
        };

        match &key_result {
            KeymapResult::Matched(command) => {
                execute_command(command);
            }
            KeymapResult::Pending(node) => cxt.editor.autoinfo = Some(node.infobox()),
            KeymapResult::MatchedSequence(commands) => {
                for command in commands {
                    execute_command(command);
                }
            }
            KeymapResult::NotFound | KeymapResult::Cancelled(_) => return Some(key_result),
        }
        None
    }

    fn insert_mode(&mut self, cx: &mut commands::Context, event: KeyEvent) {
        if let Some(keyresult) = self.handle_keymap_event(Mode::Insert, cx, event) {
            match keyresult {
                KeymapResult::NotFound => {
                    if !self.on_next_key(OnKeyCallbackKind::Fallback, cx, event) {
                        match event.typed_char() {
                            Some(ch) => commands::insert::insert_char(cx, ch),
                            // A shortcut nothing is bound to says so. In a terminal,
                            // silence is also what a key that never arrived looks like,
                            // and telling those two apart is most of the work.
                            None => cx.editor.set_status(format!("{event} is not bound")),
                        }
                    }
                }
                KeymapResult::Cancelled(pending) => {
                    for ev in pending {
                        match ev.typed_char() {
                            Some(ch) => commands::insert::insert_char(cx, ch),
                            None => {
                                if let KeymapResult::Matched(command) =
                                    self.keymaps.get(Mode::Insert, ev)
                                {
                                    command.execute(cx);
                                }
                            }
                        }
                    }
                }
                _ => unreachable!(),
            }
        }
    }

    fn command_mode(&mut self, mode: Mode, cxt: &mut commands::Context, event: KeyEvent) {
        match (event, cxt.editor.count) {
            // If the count is already started and the input is a number, always continue the count.
            (key!(i @ '0'..='9'), Some(count)) => {
                let i = i.to_digit(10).unwrap() as usize;
                let count = count.get() * 10 + i;
                if count > 100_000_000 {
                    return;
                }
                cxt.editor.count = NonZeroUsize::new(count);
            }
            // A non-zero digit will start the count if that number isn't used by a keymap.
            (key!(i @ '1'..='9'), None) if !self.keymaps.contains_key(mode, event) => {
                let i = i.to_digit(10).unwrap() as usize;
                cxt.editor.count = NonZeroUsize::new(i);
            }
            // special handling for repeat operator
            (key!('.'), _) if self.keymaps.pending().is_empty() => {
                for _ in 0..cxt.editor.count.map_or(1, NonZeroUsize::into) {
                    // first execute whatever put us into insert mode
                    self.last_insert.0.execute(cxt);
                    let mut last_savepoint = None;
                    let mut last_request_savepoint = None;
                    // then replay the inputs
                    for key in self.last_insert.1.clone() {
                        match key {
                            InsertEvent::Key(key) => self.insert_mode(cxt, key),
                            InsertEvent::CompletionApply {
                                trigger_offset,
                                changes,
                            } => {
                                let (view, doc) = current!(cxt.editor);

                                if let Some(last_savepoint) = last_savepoint.as_deref() {
                                    doc.restore(view, last_savepoint, true);
                                }

                                let text = doc.text().slice(..);
                                let cursor = doc.selection(view.id).primary().cursor(text);

                                let shift_position = |pos: usize| -> usize {
                                    (pos + cursor).saturating_sub(trigger_offset)
                                };

                                let tx = Transaction::change(
                                    doc.text(),
                                    changes.iter().cloned().map(|(start, end, t)| {
                                        (shift_position(start), shift_position(end), t)
                                    }),
                                );
                                doc.apply(&tx, view.id);
                            }
                            InsertEvent::TriggerCompletion => {
                                last_savepoint = take(&mut last_request_savepoint);
                            }
                            InsertEvent::RequestCompletion => {
                                let (view, doc) = current!(cxt.editor);
                                last_request_savepoint = Some(doc.savepoint(view));
                            }
                        }
                    }
                }
                cxt.editor.count = None;
            }
            _ => {
                // set the count
                cxt.count = cxt.editor.count;
                // TODO: edge case: 0j -> reset to 1
                // if this fails, count was Some(0)
                // debug_assert!(cxt.count != 0);

                // set the register
                cxt.register = cxt.editor.selected_register.take();

                let res = self.handle_keymap_event(mode, cxt, event);
                if matches!(&res, Some(KeymapResult::NotFound)) {
                    self.on_next_key(OnKeyCallbackKind::Fallback, cxt, event);
                }
                if self.keymaps.pending().is_empty() {
                    cxt.editor.count = None
                } else {
                    cxt.editor.selected_register = cxt.register.take();
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn set_completion(
        &mut self,
        editor: &mut Editor,
        items: Vec<CompletionItem>,
        trigger_offset: usize,
        size: Rect,
    ) -> Option<Rect> {
        let mut completion = Completion::new(editor, items, trigger_offset);

        if completion.is_empty() {
            // skip if we got no completion results
            return None;
        }

        let area = completion.area(size, editor);
        editor.last_completion = Some(CompleteAction::Triggered);
        self.last_insert.1.push(InsertEvent::TriggerCompletion);

        // TODO : propagate required size on resize to completion too
        self.completion = Some(completion);
        Some(area)
    }

    pub fn clear_completion(&mut self, editor: &mut Editor) -> Option<OnKeyCallback> {
        self.completion = None;
        let mut on_next_key: Option<OnKeyCallback> = None;
        editor.handlers.completions.request_controller.restart();
        editor.handlers.completions.active_completions.clear();
        if let Some(last_completion) = editor.last_completion.take() {
            match last_completion {
                CompleteAction::Triggered => (),
                CompleteAction::Applied {
                    trigger_offset,
                    changes,
                    placeholder,
                } => {
                    self.last_insert.1.push(InsertEvent::CompletionApply {
                        trigger_offset,
                        changes,
                    });
                    on_next_key = placeholder.then_some(Box::new(|cx, key| {
                        if let Some(c) = key.char() {
                            let (view, doc) = current!(cx.editor);
                            if let Some(snippet) = &doc.active_snippet {
                                doc.apply(&snippet.delete_placeholder(doc.text()), view.id);
                            }
                            commands::insert::insert_char(cx, c);
                        }
                    }))
                }
                CompleteAction::Selected { savepoint } => {
                    let (view, doc) = current!(editor);
                    doc.restore(view, &savepoint, false);
                }
            }
        }
        on_next_key
    }

    pub fn handle_idle_timeout(&mut self, cx: &mut commands::Context) -> EventResult {
        commands::compute_inlay_hints_for_all_views(cx.editor, cx.jobs);

        EventResult::Ignored(None)
    }
}

/// Whether the focused doc's workspace is in restricted mode and running `trust` would
/// change something visible at the workspace level.
fn workspace_trust_indicator_visible(editor: &Editor) -> bool {
    if editor.workspace_trust.implicit_level()
        == helix_loader::workspace_trust::ImplicitTrustLevel::Insecure
    {
        return false;
    }
    let (_, doc) = helix_view::current_ref!(editor);
    editor
        .workspace_trust
        .restricted_for_doc(doc.workspace_root(), doc.servers_to_load())
}

impl EditorView {
    /// must be called whenever the editor processed input that
    /// is not a `KeyEvent`. In these cases any pending keys/on next
    /// key callbacks must be canceled.
    fn handle_non_key_input(&mut self, cxt: &mut commands::Context) {
        cxt.editor.status_msg = None;
        cxt.editor.reset_idle_timer();
        // HACKS: create a fake key event that will never trigger any actual map
        // and therefore simply acts as "dismiss"
        let null_key_event = KeyEvent {
            code: KeyCode::Null,
            modifiers: KeyModifiers::empty(),
        };
        // dismiss any pending keys
        if let Some((on_next_key, _)) = self.on_next_key.take() {
            on_next_key(cxt, null_key_event);
        }
        self.handle_keymap_event(cxt.editor.mode, cxt, null_key_event);
        self.pseudo_pending.clear();
    }

    /// Takes the selection being made with the mouse to the pointer. Off the text the
    /// pointer counts as the nearest cell of the view, and above or below it as the line
    /// past the edge, so a drag that leaves the view scrolls it.
    fn drag_selection_to(
        &mut self,
        cxt: &mut commands::Context,
        row: u16,
        column: u16,
    ) -> EventResult {
        let typing = cxt.editor.mode == Mode::Insert;

        let (view, doc) = current!(cxt.editor);
        let inner = view.inner_area(doc);
        if inner.width == 0 || inner.height == 0 {
            return EventResult::Ignored(None);
        }
        let on_row = row.clamp(inner.top(), inner.bottom() - 1);
        let on_column = column.clamp(inner.left(), inner.right() - 1);
        let Some(mut pos) = view.pos_at_screen_coords(doc, on_row, on_column, false) else {
            return EventResult::Ignored(None);
        };

        let text = doc.text().slice(..);
        let line = text.char_to_line(pos);
        let beyond = if row < inner.top() {
            line.checked_sub(1)
        } else if row >= inner.bottom() && line + 1 < text.len_lines() {
            Some(line + 1)
        } else {
            None
        };
        if let Some(beyond) = beyond {
            let offset = pos - text.line_to_char(line);
            let start = text.line_to_char(beyond);
            pos = (start + offset).min(line_end_char_index(&text, beyond));
        }

        let mut selection = doc.selection(view.id).clone();
        let primary = selection.primary_mut();
        *primary = match self.drag_unit {
            // From the word or line clicked to the one under the pointer, whole.
            Some((unit, origin)) => {
                let here = unit_range(text, pos, unit);
                if here.from() >= origin.from() {
                    Range::new(origin.from(), here.to())
                } else {
                    Range::new(origin.to(), here.from())
                }
            }
            // While typing the caret is a bar between two characters, as a click puts it:
            // the selection runs from the boundary the button went down on to the one under
            // the pointer, either way, and never takes the character past it as well.
            None if typing => Range::new(primary.anchor, pos),
            None => primary.put_cursor(text, pos, true),
        };
        // A drag while typing selects, like Shift with an arrow does — but only while it
        // covers something. Pressing and letting go on the same spot is a click, and a
        // click leaves a caret: what is typed next goes in beside it, never over the
        // character it landed on.
        let selected = primary.anchor != primary.head;
        doc.set_selection(view.id, selection);
        let view_id = view.id;
        if typing {
            view_mut!(cxt.editor).insert_selection = selected;
        }
        cxt.editor.ensure_cursor_in_view(view_id);
        EventResult::Consumed(None)
    }

    fn handle_mouse_event(
        &mut self,
        event: &MouseEvent,
        cxt: &mut commands::Context,
    ) -> EventResult {
        if event.kind != MouseEventKind::Moved {
            self.handle_non_key_input(cxt)
        }

        let config = cxt.editor.config();
        let MouseEvent {
            kind,
            row,
            column,
            modifiers,
            ..
        } = *event;

        // A move stores where the pointer is and arms the timer, nothing more: the
        // terminal reports every cell crossed, and none of them is worth a frame.
        if kind == MouseEventKind::Moved && self.pointer != Some((row, column)) {
            self.pointer = Some((row, column));
            *lock(&self.pointer_moved_at) = Instant::now();
            if !self.hover_armed {
                self.hover_armed = true;
                arm_hover_timer(self.pointer_moved_at.clone());
            }
        }

        self.set_pointer_shape(row, column, cxt.editor);

        // A drag that started on the text stays the text's: over the gutter, the tree, the
        // tabs or past the edge of the screen, the selection follows the pointer, and where
        // the button is let go is where it ends.
        if self.selecting {
            match kind {
                MouseEventKind::Drag(MouseButton::Left) => {
                    return self.drag_selection_to(cxt, row, column);
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    self.selecting = false;
                    self.drag_selection_to(cxt, row, column);
                    return mouse_selection_done(cxt);
                }
                // A move with no button held: the release was never reported.
                MouseEventKind::Moved | MouseEventKind::Down(_) => self.selecting = false,
                _ => {}
            }
        }

        // A drag of the sidebar's separator stays the sidebar's when the mouse leaves it.
        if self.sidebar.contains(row, column) || self.sidebar.resizing() {
            return self.sidebar.handle_mouse(event, cxt);
        }

        // A press anywhere outside the sidebar gives the keys back to the text: whatever
        // had them, what is typed next is typed into the file.
        if matches!(kind, MouseEventKind::Down(_)) && self.sidebar.focused {
            self.sidebar.focus_code();
        }

        // A drag of the preview's separator stays the preview's when the mouse leaves it.
        if self.markdown_preview.contains(row, column) || self.markdown_preview.resizing() {
            return self.markdown_preview.handle_mouse(event, cxt);
        }

        // With nothing open, a click runs the welcome's line under it and nothing else: there
        // is no text to select.
        if cxt.editor.nothing_open() && !self.sidebar.code_hidden() {
            if kind == MouseEventKind::Down(MouseButton::Left) {
                if let Some(command) = self.welcome.command_at(row, column) {
                    return EventResult::Consumed(Some(Box::new(move |compositor, cx| {
                        run_command(compositor, cx, command)
                    })));
                }
            }
            return EventResult::Consumed(None);
        }

        // A split separator is taken before the views see the press, and while it is dragged
        // the mouse moves it instead of selecting text.
        if let Some(separator) = self.dragged_separator {
            match kind {
                MouseEventKind::Drag(MouseButton::Left) => {
                    if !cxt.editor.tree.drag_separator(separator, row, column) {
                        self.dragged_separator = None;
                    }
                    return EventResult::Consumed(None);
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    self.dragged_separator = None;
                    return EventResult::Consumed(None);
                }
                _ => self.dragged_separator = None,
            }
        }
        if kind == MouseEventKind::Down(MouseButton::Left) {
            if let Some(separator) = cxt.editor.tree.separator_at(row, column) {
                self.dragged_separator = Some(separator);
                return EventResult::Consumed(None);
            }
        }

        if kind == MouseEventKind::Down(MouseButton::Left) {
            if self
                .bufferline_back
                .is_some_and(|mark| hits(mark, row, column))
            {
                self.bufferline_first = self.bufferline_first.saturating_sub(1);
                return EventResult::Consumed(None);
            }
            if self
                .bufferline_forward
                .is_some_and(|mark| hits(mark, row, column))
            {
                self.bufferline_first += 1;
                return EventResult::Consumed(None);
            }
        }

        // The wheel over the tabs walks them, the way a click on one opens it.
        if hits(self.bufferline_area, row, column) {
            match kind {
                MouseEventKind::ScrollUp => {
                    commands::MappableCommand::goto_previous_buffer.execute(cxt);
                    return EventResult::Consumed(None);
                }
                MouseEventKind::ScrollDown => {
                    commands::MappableCommand::goto_next_buffer.execute(cxt);
                    return EventResult::Consumed(None);
                }
                _ => {}
            }
        }

        if kind == MouseEventKind::Down(MouseButton::Left) {
            match self.bufferline_hit(row, column) {
                Some(BufferlineHit::Open(doc_id)) => {
                    // Going to a file closes the preview: it is what the screen was showing.
                    self.markdown_preview.full = false;
                    cxt.editor
                        .switch(doc_id, helix_view::editor::Action::Replace);
                    return EventResult::Consumed(None);
                }
                Some(BufferlineHit::ClosePreview) => {
                    self.markdown_preview.full = false;
                    return EventResult::Consumed(None);
                }
                Some(BufferlineHit::Close(doc_id)) => {
                    return close_bufferline_tab(cxt, doc_id);
                }
                None => {}
            }
        }

        let pos_and_view = |editor: &Editor, row, column, ignore_virtual_text| {
            editor.tree.views().find_map(|(view, _focus)| {
                view.pos_at_screen_coords(
                    &editor.documents[&view.doc],
                    row,
                    column,
                    ignore_virtual_text,
                )
                .map(|pos| (pos, view.id))
            })
        };

        let gutter_coords_and_view = |editor: &Editor, row, column| {
            editor.tree.views().find_map(|(view, _focus)| {
                view.gutter_coords_at_screen_coords(row, column)
                    .map(|coords| (coords, view.id))
            })
        };

        // The right button opens what can be done to what is under it.
        if kind == MouseEventKind::Down(MouseButton::Right) {
            if let Some(BufferlineHit::Open(doc_id) | BufferlineHit::Close(doc_id)) =
                self.bufferline_hit(row, column)
            {
                return open_tab_menu(row, column, doc_id);
            }

            if let Some((pos, view_id)) = pos_and_view(cxt.editor, row, column, true) {
                self.sidebar.focus_code();
                cxt.editor.focus(view_id);

                // Pointing outside the selection takes the caret there first: the menu acts
                // on what was pointed at, which is what every other editor does.
                let (view, doc) = current!(cxt.editor);
                let pointed = doc
                    .selection(view.id)
                    .ranges()
                    .iter()
                    .any(|range| range.from() <= pos && pos < range.to());
                if !pointed {
                    doc.set_selection(view.id, Selection::point(pos));
                }

                return open_editor_menu(row, column);
            }
            // The gutter and blank space still belong to the code panel.
            let view = cxt
                .editor
                .tree
                .views()
                .find(|(view, _)| {
                    row >= view.area.y
                        && row < view.area.bottom()
                        && column >= view.area.x
                        && column < view.area.right()
                })
                .map(|(view, _)| view.id);
            if let Some(view) = view {
                self.sidebar.focus_code();
                cxt.editor.focus(view);
                return open_editor_menu(row, column);
            }
        }

        match kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // Held with Ctrl, or Cmd where the terminal passes it on, a click goes to
                // the definition of what it landed on. The caret goes there first, since
                // that is what the request is made for; nothing else moves.
                let goto = modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER);
                if goto {
                    let Some((pos, view_id)) = pos_and_view(cxt.editor, row, column, true) else {
                        return EventResult::Ignored(None);
                    };

                    cxt.editor.focus(view_id);
                    let doc = doc_mut!(cxt.editor, &view!(cxt.editor, view_id).doc);
                    doc.set_selection(view_id, Selection::point(pos));
                    cxt.editor.ensure_cursor_in_view(view_id);
                    commands::MappableCommand::goto_definition.execute(cxt);

                    return EventResult::Consumed(None);
                }

                let now = Instant::now();
                let count = match self.last_click {
                    Some(click)
                        if click.row == row
                            && click.column == column
                            && now.duration_since(click.at) < DOUBLE_CLICK =>
                    {
                        click.count % 3 + 1
                    }
                    _ => 1,
                };
                self.drag_unit = None;

                let editor = &mut cxt.editor;

                if let Some((pos, view_id)) = pos_and_view(editor, row, column, true) {
                    editor.focus(view_id);
                    self.selecting = true;
                    self.last_click = Some(Click {
                        at: now,
                        row,
                        column,
                        count,
                    });

                    // A click that lands far from the cursor is a jump: leaving by clicking
                    // is still leaving, and coming back is what the back key is for.
                    let (view, doc) = current!(editor);
                    let far = {
                        let text = doc.text().slice(..);
                        let cursor = doc.selection(view.id).primary().cursor(text);

                        text.char_to_line(cursor).abs_diff(text.char_to_line(pos)) >= JUMP_LINES
                    };
                    if far {
                        commands::push_jump(view, doc);
                    }

                    let prev_view_id = view!(editor).id;
                    let doc = doc_mut!(editor, &view!(editor, view_id).doc);

                    if count > 1 && modifiers.is_empty() {
                        // A second click takes the word, a third the line, and a drag from
                        // there grows by the same unit. Typing over it replaces it, as a
                        // selection made with Shift and an arrow does.
                        let unit = if count == 2 {
                            ClickUnit::Word
                        } else {
                            ClickUnit::Line
                        };
                        let range = unit_range(doc.text().slice(..), pos, unit);
                        doc.set_selection(view_id, Selection::single(range.anchor, range.head));
                        self.drag_unit = Some((unit, range));
                        commands::mark_insert_selection(editor);
                    } else if modifiers == KeyModifiers::ALT {
                        let selection = doc.selection(view_id).clone();
                        doc.set_selection(view_id, selection.push(Range::point(pos)));
                    } else if editor.mode == Mode::Select {
                        // Discards non-primary selections for consistent UX with normal mode
                        let primary = doc.selection(view_id).primary().put_cursor(
                            doc.text().slice(..),
                            pos,
                            true,
                        );
                        editor.mouse_down_range = Some(primary);
                        doc.set_selection(view_id, Selection::single(primary.anchor, primary.head));
                    } else {
                        doc.set_selection(view_id, Selection::point(pos));
                        // A click selects nothing, whatever was selected before it: the
                        // caret it leaves is a caret, and typing does not replace the
                        // character it sits on.
                        view_mut!(editor, view_id).insert_selection = false;
                    }

                    if view_id != prev_view_id {
                        self.clear_completion(editor);
                    }

                    editor.ensure_cursor_in_view(view_id);

                    return EventResult::Consumed(None);
                }

                if let Some((coords, view_id)) = gutter_coords_and_view(editor, row, column) {
                    editor.focus(view_id);

                    let (view, doc) = current!(cxt.editor);

                    let Some(path) = doc.path().map(ToOwned::to_owned) else {
                        return EventResult::Ignored(None);
                    };

                    if let Some(char_idx) =
                        view.pos_at_visual_coords(doc, coords.row as u16, coords.col as u16, true)
                    {
                        let line = doc.text().char_to_line(char_idx);
                        commands::dap_toggle_breakpoint_impl(cxt, path, line);
                        return EventResult::Consumed(None);
                    }
                }

                EventResult::Ignored(None)
            }

            MouseEventKind::Drag(MouseButton::Left) => self.drag_selection_to(cxt, row, column),

            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let current_view = cxt.editor.tree.focus;

                let direction = match event.kind {
                    MouseEventKind::ScrollUp => Direction::Backward,
                    MouseEventKind::ScrollDown => Direction::Forward,
                    _ => unreachable!(),
                };

                match pos_and_view(cxt.editor, row, column, false) {
                    Some((_, view_id)) => cxt.editor.tree.focus = view_id,
                    None => return EventResult::Ignored(None),
                }

                let offset = config.scroll_lines.unsigned_abs();
                commands::scroll(cxt, offset, direction, false);

                cxt.editor.tree.focus = current_view;
                cxt.editor.ensure_cursor_in_view(current_view);

                EventResult::Consumed(None)
            }

            MouseEventKind::Up(MouseButton::Left) => mouse_selection_done(cxt),

            MouseEventKind::Up(MouseButton::Right) => {
                if let Some((pos, view_id)) = gutter_coords_and_view(cxt.editor, row, column) {
                    cxt.editor.focus(view_id);

                    if let Some((pos, _)) = pos_and_view(cxt.editor, row, column, true) {
                        doc_mut!(cxt.editor).set_selection(view_id, Selection::point(pos));
                    } else {
                        let (view, doc) = current!(cxt.editor);

                        if let Some(pos) = view.pos_at_visual_coords(doc, pos.row as u16, 0, true) {
                            doc.set_selection(view_id, Selection::point(pos));
                            match modifiers {
                                KeyModifiers::ALT => {
                                    commands::MappableCommand::dap_edit_log.execute(cxt)
                                }
                                _ => commands::MappableCommand::dap_edit_condition.execute(cxt),
                            };
                        }
                    }

                    cxt.editor.ensure_cursor_in_view(view_id);
                    return EventResult::Consumed(None);
                }
                EventResult::Ignored(None)
            }

            MouseEventKind::Up(MouseButton::Middle) => {
                let editor = &mut cxt.editor;
                if !config.middle_click_paste {
                    return EventResult::Ignored(None);
                }

                if modifiers == KeyModifiers::ALT {
                    commands::replace_selections_with_register(
                        cxt.editor,
                        config.mouse_yank_register,
                        cxt.count(),
                    );

                    return EventResult::Consumed(None);
                }

                if let Some((pos, view_id)) = pos_and_view(editor, row, column, true) {
                    let doc = doc_mut!(editor, &view!(editor, view_id).doc);
                    doc.set_selection(view_id, Selection::point(pos));
                    cxt.editor.focus(view_id);

                    commands::paste(
                        cxt.editor,
                        config.mouse_yank_register,
                        commands::Paste::Before,
                        cxt.count(),
                    );

                    return EventResult::Consumed(None);
                }

                EventResult::Ignored(None)
            }

            _ => EventResult::Ignored(None),
        }
    }
    fn on_next_key(
        &mut self,
        kind: OnKeyCallbackKind,
        ctx: &mut commands::Context,
        event: KeyEvent,
    ) -> bool {
        if let Some((on_next_key, kind_)) = self.on_next_key.take() {
            if kind == kind_ {
                on_next_key(ctx, event);
                true
            } else {
                self.on_next_key = Some((on_next_key, kind_));
                false
            }
        } else {
            false
        }
    }
}

impl EditorView {
    /// A text beam over text and an arrow over everything else — the sidebar, the tabs,
    /// the welcome screen — asked of the terminal with OSC 22. A terminal that does not
    /// know it leaves the pointer as it was.
    fn set_pointer_shape(&mut self, row: u16, column: u16, editor: &Editor) {
        let over_text = !self.sidebar.contains(row, column)
            && !self.markdown_preview.contains(row, column)
            && !editor.nothing_open()
            && editor.tree.views().any(|(view, _)| {
                let area = view.area;
                // The last row of a view is its status line.
                row >= area.y
                    && row + 1 < area.bottom()
                    && column >= area.x
                    && column < area.right()
            });
        let shape = if over_text { "text" } else { "default" };
        if self.pointer_shape == Some(shape) {
            return;
        }
        self.pointer_shape = Some(shape);
        write_pointer_shape(shape);
    }
}

/// Writes an OSC 22 pointer shape; an empty one gives the terminal its own back.
pub fn write_pointer_shape(shape: &str) {
    if cfg!(feature = "integration") {
        return;
    }
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    let _ = write!(stdout, "\x1b]22;{shape}\x1b\\");
    let _ = stdout.flush();
}

impl Component for EditorView {
    fn handle_event(
        &mut self,
        event: &Event,
        context: &mut crate::compositor::Context,
    ) -> EventResult {
        let mut cx = commands::Context {
            editor: context.editor,
            count: None,
            register: None,
            callback: Vec::new(),
            on_next_key_callback: None,
            jobs: context.jobs,
        };

        match event {
            Event::Paste(contents) => {
                cx.editor.registers.clipboard_pasted(contents);
                self.handle_non_key_input(&mut cx);
                cx.count = cx.editor.count;
                commands::paste_bracketed_value(&mut cx, contents.clone());
                cx.editor.count = None;

                let config = cx.editor.config();
                let mode = cx.editor.mode();
                let (view, doc) = current!(cx.editor);
                view.ensure_cursor_in_view(doc, config.scrolloff);

                // Store a history state if not in insert mode. Otherwise wait till we exit insert
                // to include any edits to the paste in the history state.
                if mode != Mode::Insert {
                    doc.append_changes_to_history(view);
                }

                EventResult::Consumed(None)
            }
            Event::Resize(_width, _height) => {
                // Ignore this event, we handle resizing just before rendering to screen.
                // Handling it here but not re-rendering will cause flashing
                EventResult::Consumed(None)
            }
            Event::Key(mut key) => {
                cx.editor.reset_idle_timer();
                canonicalize_key(&mut key);

                // clear status
                cx.editor.status_msg = None;

                let mode = cx.editor.mode();

                // The preview filling the screen takes the key first: it scrolls and closes
                // on its own, and swallows the rest so no command edits the file nobody
                // can see. A shortcut carries a modifier and goes on to the keymap.
                if self.markdown_preview.is_full(cx.editor) && self.on_next_key.is_none() {
                    if let EventResult::Consumed(_) =
                        self.markdown_preview.handle_key(key, cx.editor)
                    {
                        return EventResult::Consumed(None);
                    }
                }

                // A key the sidebar passes on is the editor's, and goes on to the keymap below.
                if self.sidebar.focused && self.on_next_key.is_none() {
                    if let EventResult::Consumed(_) = self.sidebar.handle_key(key, &mut cx) {
                        // A prompt the sidebar opened rides on the callbacks, like a command's.
                        let callbacks = take(&mut cx.callback);
                        if callbacks.is_empty() {
                            return EventResult::Consumed(None);
                        }
                        let callback: crate::compositor::Callback =
                            Box::new(move |compositor, cx| {
                                for callback in callbacks {
                                    callback(compositor, cx)
                                }
                            });
                        return EventResult::Consumed(Some(callback));
                    }
                }

                // Nothing is open to type into: a plain key would only fill the blank behind
                // the welcome. A shortcut goes on to the keymap. Where insert is a mode you
                // enter, entering it is asking to type, and the typing goes through.
                if mode == Mode::Insert
                    && cx.editor.config().default_mode == Mode::Insert
                    && self.on_next_key.is_none()
                    && !self.sidebar.focused
                    && cx.editor.nothing_open()
                    && !sidebar::is_editor_shortcut(key)
                {
                    return EventResult::Consumed(None);
                }

                if !self.on_next_key(OnKeyCallbackKind::PseudoPending, &mut cx, key) {
                    match mode {
                        Mode::Insert => {
                            // let completion swallow the event if necessary
                            let mut consumed = false;
                            if let Some(completion) = &mut self.completion {
                                let res = {
                                    // use a fake context here
                                    let mut cx = Context {
                                        editor: cx.editor,
                                        jobs: cx.jobs,
                                        scroll: None,
                                    };

                                    if let EventResult::Consumed(callback) =
                                        completion.handle_event(event, &mut cx)
                                    {
                                        consumed = true;
                                        Some(callback)
                                    } else if let EventResult::Consumed(callback) =
                                        completion.handle_event(&Event::Key(key!(Enter)), &mut cx)
                                    {
                                        Some(callback)
                                    } else {
                                        None
                                    }
                                };

                                if let Some(callback) = res {
                                    if callback.is_some() {
                                        // assume close_fn
                                        if let Some(cb) = self.clear_completion(cx.editor) {
                                            if consumed {
                                                cx.on_next_key_callback =
                                                    Some((cb, OnKeyCallbackKind::Fallback))
                                            } else {
                                                self.on_next_key =
                                                    Some((cb, OnKeyCallbackKind::Fallback));
                                            }
                                        }
                                    }
                                }
                            }

                            // if completion didn't take the event, we pass it onto commands
                            if !consumed {
                                self.insert_mode(&mut cx, key);

                                // record last_insert key
                                self.last_insert.1.push(InsertEvent::Key(key));
                            }
                        }
                        mode => self.command_mode(mode, &mut cx, key),
                    }
                }

                self.on_next_key = cx.on_next_key_callback.take();
                match self.on_next_key {
                    Some((_, OnKeyCallbackKind::PseudoPending)) => self.pseudo_pending.push(key),
                    _ => self.pseudo_pending.clear(),
                }

                // appease borrowck
                let callbacks = take(&mut cx.callback);

                // if the command consumed the last view, skip the render.
                // on the next loop cycle the Application will then terminate.
                if cx.editor.should_close() {
                    return EventResult::Ignored(None);
                }

                let config = cx.editor.config();
                let mode = cx.editor.mode();

                let (view, doc) = current!(cx.editor);

                view.ensure_cursor_in_view(doc, config.scrolloff);

                // Store a history state if not in insert mode. This also takes care of
                // committing changes when leaving insert mode.
                if mode != Mode::Insert {
                    doc.append_changes_to_history(view);
                }
                let callback = if callbacks.is_empty() {
                    None
                } else {
                    let callback: crate::compositor::Callback = Box::new(move |compositor, cx| {
                        for callback in callbacks {
                            callback(compositor, cx)
                        }
                    });
                    Some(callback)
                };

                EventResult::Consumed(callback)
            }

            Event::Mouse(event) => self.handle_mouse_event(event, &mut cx),
            Event::IdleTimeout => self.handle_idle_timeout(&mut cx),
            Event::FocusGained => {
                self.terminal_focused = true;
                EventResult::Consumed(None)
            }
            Event::FocusLost => {
                context.editor.registers.terminal_left();
                if context.editor.config().auto_save.focus_lost {
                    let options = commands::WriteAllOptions {
                        force: false,
                        write_scratch: false,
                        auto_format: false,
                        code_actions: false,
                    };
                    if let Err(e) = commands::typed::write_all_impl(context, options) {
                        context.editor.set_error(format!("{}", e));
                    }
                }
                self.terminal_focused = false;
                EventResult::Consumed(None)
            }
        }
    }

    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        // clear with background color
        surface.set_style(area, cx.editor.theme.get("ui.background"));
        let config = cx.editor.config();

        // check if bufferline should be rendered
        use helix_view::editor::BufferLine;
        let use_bufferline = match config.bufferline {
            BufferLine::Always => true,
            BufferLine::Multiple if cx.editor.documents.len() > 1 => true,
            _ => false,
        };

        // -1 for commandline and the bufferline's rows
        let mut editor_area = area.clip_bottom(1);
        let code_hidden = self.sidebar.code_hidden();
        if self.sidebar.open {
            let sidebar_width = if code_hidden {
                area.width
            } else {
                self.sidebar
                    .width(config.sidebar.width, area.width)
                    .min(area.width.saturating_sub(sidebar::EDITOR_ROOM))
            };
            let sidebar_area = editor_area.with_width(sidebar_width);
            self.sidebar.render(sidebar_area, surface, cx.editor);
            if !code_hidden {
                editor_area = editor_area.clip_left(sidebar_width);
            }
        }
        let full_preview = self.markdown_preview.is_full(cx.editor);
        let preview_width = if full_preview || code_hidden {
            0
        } else {
            self.markdown_preview.width(cx.editor, editor_area)
        };
        let preview_area = editor_area.clip_left(editor_area.width - preview_width);
        editor_area = editor_area.clip_right(preview_width);
        if use_bufferline {
            editor_area = editor_area.clip_top(bufferline_height(&cx.editor.theme));
        }

        // if the terminal size suddenly changed, we need to trigger a resize
        cx.editor.resize(editor_area);

        let welcome = !code_hidden && !full_preview && cx.editor.nothing_open();
        if use_bufferline && !code_hidden && !welcome {
            let bufferline_area = Rect::new(
                editor_area.x,
                area.y,
                editor_area.width,
                bufferline_height(&cx.editor.theme),
            );
            self.render_bufferline(cx.editor, bufferline_area, surface);
        } else {
            self.clear_bufferline();
        }

        // The preview on its own takes the room the views would have had, and they are
        // not drawn at all: the file is behind it.
        if code_hidden {
            self.markdown_preview.hide();
        } else if welcome {
            self.markdown_preview.hide();
            let keymaps = self.keymaps.map();
            self.welcome
                .render(editor_area, surface, cx.editor, &keymaps);
        } else if full_preview {
            self.markdown_preview
                .render_full(editor_area, surface, cx.editor);
        } else {
            self.sidebar.follow_diff(cx.editor);
            for (view, is_focused) in cx.editor.tree.views() {
                let doc = cx.editor.document(view.doc).unwrap();
                self.render_view(cx.editor, doc, view, editor_area, surface, is_focused);
            }

            // After the views, so it follows where they scrolled this frame.
            if preview_width > 0 {
                self.markdown_preview
                    .render(preview_area, surface, cx.editor);
            } else {
                self.markdown_preview.hide();
            }
        }

        if config.auto_info {
            if let Some(mut info) = cx.editor.autoinfo.take() {
                info.render(area, surface, cx);
                cx.editor.autoinfo = Some(info)
            }
        }

        let key_width = 15u16; // for showing pending keys
        let mut status_msg_width = 0;

        // render status msg
        if let Some((status_msg, severity)) = &cx.editor.status_msg {
            status_msg_width = status_msg.width();
            use helix_view::editor::Severity;
            let style = if *severity == Severity::Error {
                cx.editor.theme.get("error")
            } else {
                cx.editor.theme.get("ui.text")
            };

            surface.set_string(
                area.x,
                area.y + area.height.saturating_sub(1),
                status_msg,
                style,
            );
        }

        if area.width.saturating_sub(status_msg_width as u16) > key_width {
            let mut disp = String::new();
            if let Some(count) = cx.editor.count {
                disp.push_str(&count.to_string())
            }
            for key in self.keymaps.pending() {
                disp.push_str(&key.key_sequence_format());
            }
            for key in &self.pseudo_pending {
                disp.push_str(&key.key_sequence_format());
            }
            let style = cx.editor.theme.get("ui.text");
            let macro_width = if cx.editor.macro_recording.is_some() {
                3
            } else {
                0
            };
            let restricted = workspace_trust_indicator_visible(cx.editor);
            let trust_width = if restricted { 3 } else { 0 };
            surface.set_string(
                area.x
                    + area
                        .width
                        .saturating_sub(key_width + macro_width + trust_width),
                area.y + area.height.saturating_sub(1),
                disp.get(disp.len().saturating_sub(key_width as usize)..)
                    .unwrap_or(&disp),
                style,
            );
            if restricted {
                let style = style
                    .fg(helix_view::graphics::Color::Yellow)
                    .add_modifier(Modifier::BOLD);
                surface.set_string(
                    area.x
                        .saturating_add(area.width.saturating_sub(3 + macro_width)),
                    area.y + area.height.saturating_sub(1),
                    "[⚠]",
                    style,
                );
            }
            if let Some((reg, _)) = cx.editor.macro_recording {
                let disp = format!("[{}]", reg);
                let style = style
                    .fg(helix_view::graphics::Color::Yellow)
                    .add_modifier(Modifier::BOLD);
                surface.set_string(
                    area.x + area.width.saturating_sub(3),
                    area.y + area.height.saturating_sub(1),
                    &disp,
                    style,
                );
            }
        }

        if let Some(completion) = self.completion.as_mut() {
            completion.render(area, surface, cx);
        }
    }

    fn cursor(&self, _area: Rect, editor: &Editor) -> (Option<Position>, CursorKind) {
        // Nothing on screen is being edited while the preview has it to itself.
        if self.sidebar.focused || self.markdown_preview.full || editor.nothing_open() {
            return (None, CursorKind::Hidden);
        }

        match editor.cursor() {
            // all block cursors are drawn manually
            (pos, CursorKind::Block) => {
                if self.terminal_focused && editor.config().cursor_blink {
                    (pos, CursorKind::Block)
                } else if self.terminal_focused {
                    (pos, CursorKind::Hidden)
                } else {
                    // use terminal cursor when terminal loses focus
                    (pos, CursorKind::Underline)
                }
            }
            cursor => cursor,
        }
    }
}

/// Runs one of the editor's own commands from a menu.
fn run_command(
    compositor: &mut Compositor,
    outer: &mut compositor::Context,
    command: MappableCommand,
) {
    context_menu::with_context(compositor, outer, |cx| command.execute(cx));
}

/// Split the file that was pointed at, without replacing the current view.
fn open_tab_menu(row: u16, column: u16, doc_id: helix_view::DocumentId) -> EventResult {
    EventResult::Consumed(Some(Box::new(move |compositor, cx| {
        cx.editor
            .switch(doc_id, helix_view::editor::Action::Replace);
        let mut entries: Vec<_> = [
            (
                "Split vertically",
                helix_view::editor::Action::VerticalSplit,
            ),
            (
                "Split horizontally",
                helix_view::editor::Action::HorizontalSplit,
            ),
        ]
        .into_iter()
        .map(|(label, action)| {
            context_menu::Entry::new(
                label,
                "",
                Box::new(move |compositor, cx| {
                    let view = compositor.find::<EditorView>().unwrap();
                    view.markdown_preview.full = false;
                    view.sidebar.focus_code();
                    cx.editor.switch(doc_id, action);
                }),
            )
        })
        .collect();
        // The tab pointed at is the current one now, so "others" are all but it.
        for (label, keys, command) in [
            ("Close", "Ctrl+w", "buffer-close"),
            ("Close other tabs", "", "buffer-close-others"),
            ("Close all tabs", "Shift+F4", "buffer-close-all"),
        ] {
            entries.push(context_menu::Entry::new(
                label,
                keys,
                Box::new(move |_compositor, cx| commands::run_typable(cx, command)),
            ));
        }
        entries.extend(review_menu_entries(compositor, cx));
        let menu = context_menu::ContextMenu::new((row, column), entries);
        compositor.push(Box::new(menu));
    })))
}

/// The same panel controls are reachable from either side, including when one is hidden.
fn review_menu_entries(
    compositor: &mut Compositor,
    cx: &compositor::Context,
) -> Vec<context_menu::Entry> {
    let view = compositor.find::<EditorView>().unwrap();
    let commits = view.sidebar.showing(sidebar::TabKind::Commits);
    let hidden = view.sidebar.code_hidden();
    let full = view.sidebar.full_context();
    let beside = view.sidebar.side_by_side();
    let files = view.sidebar.files_visible();
    let mut entries = vec![
        context_menu::Entry::new(
            if commits {
                "Hide commits panel"
            } else {
                "Show commits panel"
            },
            "F6",
            Box::new(|compositor, cx| {
                run_command(compositor, cx, MappableCommand::review_commits_toggle)
            }),
        ),
        context_menu::Entry::new(
            if hidden {
                "Show code panel"
            } else {
                "Hide code panel"
            },
            "F7",
            Box::new(|compositor, cx| {
                run_command(compositor, cx, MappableCommand::review_code_toggle)
            }),
        ),
    ];
    entries.push(context_menu::Entry::new(
        if files {
            "Hide commit files panel"
        } else {
            "Show commit files panel"
        },
        "F9",
        Box::new(|compositor, cx| {
            run_command(compositor, cx, MappableCommand::review_files_toggle)
        }),
    ));
    if doc!(cx.editor).review.is_some() {
        entries.push(context_menu::Entry::new(
            if full {
                "Show changed sections only"
            } else {
                "Show full file context"
            },
            "F4",
            Box::new(|compositor, cx| {
                run_command(compositor, cx, MappableCommand::review_context_toggle)
            }),
        ));
        entries.push(context_menu::Entry::new(
            if beside {
                "Show one side above the other"
            } else {
                "Show side by side"
            },
            "Ctrl-Alt-d",
            Box::new(|compositor, cx| {
                run_command(compositor, cx, MappableCommand::review_side_by_side_toggle)
            }),
        ));
    }
    entries.push(context_menu::Entry::new(
        "Keyboard shortcuts",
        "F1",
        Box::new(|compositor, cx| run_command(compositor, cx, MappableCommand::keyboard_shortcuts)),
    ));
    entries
}

pub(super) fn open_review_menu(row: u16, column: u16) -> EventResult {
    EventResult::Consumed(Some(Box::new(move |compositor, cx| {
        let entries = review_menu_entries(compositor, cx);
        compositor.push(Box::new(context_menu::ContextMenu::new(
            (row, column),
            entries,
        )));
    })))
}

/// What can be done to the text under the pointer.
fn open_editor_menu(row: u16, column: u16) -> EventResult {
    EventResult::Consumed(Some(Box::new(move |compositor, cx| {
        let review = doc!(cx.editor).review.is_some();
        let mut entries = if review {
            vec![context_menu::Entry::new(
                "Copy",
                "Ctrl-c",
                Box::new(|compositor, cx| {
                    run_command(compositor, cx, MappableCommand::copy_to_clipboard)
                }),
            )]
        } else {
            vec![
                context_menu::Entry::new(
                    "Cut",
                    "Ctrl-x",
                    Box::new(|compositor, cx| {
                        run_command(compositor, cx, MappableCommand::cut_to_clipboard)
                    }),
                ),
                context_menu::Entry::new(
                    "Copy",
                    "Ctrl-c",
                    Box::new(|compositor, cx| {
                        run_command(compositor, cx, MappableCommand::copy_to_clipboard)
                    }),
                ),
                context_menu::Entry::new(
                    "Paste",
                    "Ctrl-v",
                    Box::new(|compositor, cx| {
                        run_command(compositor, cx, MappableCommand::paste_from_clipboard)
                    }),
                ),
                context_menu::Entry::new(
                    "Go to the definition",
                    "F12",
                    Box::new(|compositor, cx| {
                        run_command(compositor, cx, MappableCommand::goto_definition)
                    }),
                ),
                context_menu::Entry::new(
                    "Rename the symbol",
                    "F2",
                    Box::new(|compositor, cx| {
                        run_command(compositor, cx, MappableCommand::rename_symbol)
                    }),
                ),
                context_menu::Entry::new(
                    "Split vertically",
                    "",
                    Box::new(|compositor, cx| run_command(compositor, cx, MappableCommand::vsplit)),
                ),
                context_menu::Entry::new(
                    "Split horizontally",
                    "",
                    Box::new(|compositor, cx| run_command(compositor, cx, MappableCommand::hsplit)),
                ),
            ]
        };
        if review {
            entries.push(context_menu::Entry::new(
                "Split vertically",
                "",
                Box::new(|compositor, cx| run_command(compositor, cx, MappableCommand::vsplit)),
            ));
            entries.push(context_menu::Entry::new(
                "Split horizontally",
                "",
                Box::new(|compositor, cx| run_command(compositor, cx, MappableCommand::hsplit)),
            ));
        }
        entries.extend(review_menu_entries(compositor, cx));

        // Closing the last view exits the editor; this menu only closes a split.
        if cx.editor.tree.views().count() > 1 {
            let close = context_menu::Entry::new(
                "Close split",
                "",
                Box::new(|compositor, cx| run_command(compositor, cx, MappableCommand::wclose)),
            );
            entries.push(close);
        }

        compositor.push(Box::new(context_menu::ContextMenu::new(
            (row, column),
            entries,
        )));
    })))
}

fn canonicalize_key(key: &mut KeyEvent) {
    if let KeyEvent {
        code: KeyCode::Char(ch),
        modifiers,
    } = key
    {
        // A kitty-protocol terminal may report Shift+e as `e` plus SHIFT rather than `E`.
        if modifiers.contains(KeyModifiers::SHIFT) && ch.is_lowercase() {
            let mut upper = ch.to_uppercase();
            if let (Some(first), None) = (upper.next(), upper.next()) {
                *ch = first;
            }
        }
        modifiers.remove(KeyModifiers::SHIFT)
    }
}

/// Paints one cell as two halves, `upper` over `lower`: a terminal colours no less
/// than a cell, so half a row of padding is a block glyph in two colours. A colour
/// the theme leaves unset is the terminal's own background.
fn draw_half_block(
    surface: &mut Surface,
    x: u16,
    y: u16,
    upper: Option<Color>,
    lower: Option<Color>,
) {
    let Some(cell) = surface.get_mut(x, y) else {
        return;
    };

    if upper == lower {
        cell.set_symbol(" ").set_bg(upper.unwrap_or(Color::Reset));
        return;
    }

    match lower {
        Some(lower) => {
            cell.set_symbol("▄")
                .set_fg(lower)
                .set_bg(upper.unwrap_or(Color::Reset));
        }
        // They differ, so the upper half is the one with a colour.
        None => {
            cell.set_symbol("▀")
                .set_fg(upper.unwrap_or(Color::Reset))
                .set_bg(Color::Reset);
        }
    }
}

/// Closes the buffer whose cross was clicked, as `:buffer-close` would: one with
/// unsaved changes stays open and says why.
fn close_bufferline_tab(cx: &mut commands::Context, doc_id: helix_view::DocumentId) -> EventResult {
    // A tab with a file behind it is written rather than refused; one without a file is
    // a question, since closing it is the only way its changes can be lost.
    match cx.editor.document(doc_id) {
        Some(doc) if doc.is_modified() && doc.path().is_some() => {
            if let Err(err) = cx.editor.save::<std::path::PathBuf>(doc_id, None, false) {
                log::error!("Could not save a buffer before closing it: {err:#}");
                cx.editor.set_error(format!("Could not save: {err:#}"));
                return EventResult::Consumed(None);
            }
        }
        Some(doc) if doc.is_modified() => {
            let name = doc.display_name().to_string();
            let lines = vec![format!("{name} has changes and no file to write them to.")];
            let answers = vec![
                crate::ui::confirm::Answer::new(
                    "Close without saving",
                    Box::new(move |cx: &mut crate::compositor::Context| {
                        if cx.editor.close_document(doc_id, true).is_err() {
                            log::error!("A buffer the bufferline offered could not be closed");
                            cx.editor.set_error("That buffer could not be closed");
                        }
                    }),
                )
                .destructive(),
                crate::ui::confirm::Answer::new("Cancel", Box::new(|_| {})),
            ];
            let dialog = crate::ui::confirm::Confirm::new("Unsaved changes", lines, answers);

            return EventResult::Consumed(Some(Box::new(move |compositor, _| {
                compositor.push(Box::new(dialog));
            })));
        }
        _ => {}
    }

    if let Err(err) = cx.block_try_flush_writes() {
        log::error!("Could not finish pending writes before closing a buffer: {err:#}");
        cx.editor
            .set_error(format!("Could not close the buffer: {err:#}"));
        return EventResult::Consumed(None);
    }

    match cx.editor.close_document(doc_id, false) {
        Ok(()) => {}
        Err(CloseError::BufferModified(name)) => {
            cx.editor.set_error(format!(
                "{name} has unsaved changes: write it, or :buffer-close! to drop them"
            ));
        }
        Err(CloseError::DoesNotExist) => {
            log::error!("The bufferline offered a buffer that no longer exists: {doc_id:?}");
            cx.editor.set_error("That buffer no longer exists");
        }
        Err(CloseError::SaveError(err)) => {
            log::error!("Could not close a buffer: {err:#}");
            cx.editor
                .set_error(format!("Could not close the buffer: {err:#}"));
        }
    }

    EventResult::Consumed(None)
}

/// Runs `work` off the main thread and, when it is done, hands what it made to `land` on
/// the main one, with the editor and this view. It is how anything asked of git or the
/// disk reaches the screen: as a job of the editor's, never decided while drawing.
pub(crate) fn background<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
    land: impl FnOnce(&mut Editor, &mut EditorView, T) + Send + 'static,
) {
    tokio::spawn(async move {
        let answer = match tokio::task::spawn_blocking(work).await {
            Ok(answer) => answer,
            Err(err) => {
                log::error!("a background task stopped without answering: {err}");
                return;
            }
        };
        crate::job::dispatch(move |editor, compositor| {
            let Some(view) = compositor.find::<EditorView>() else {
                return;
            };
            land(editor, view, answer);
        })
        .await;
    });
}

/// Calls `then` with the editor and this view after `delay`.
pub(crate) fn later(
    delay: std::time::Duration,
    then: impl FnOnce(&mut Editor, &mut EditorView) + Send + 'static,
) {
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        crate::job::dispatch(move |editor, compositor| {
            let Some(view) = compositor.find::<EditorView>() else {
                return;
            };
            then(editor, view);
        })
        .await;
    });
}

/// An instant cannot be left half-written, so a lock poisoned by a panic elsewhere is
/// still worth reading.
fn lock(moved_at: &Mutex<Instant>) -> MutexGuard<'_, Instant> {
    moved_at
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Waits until the pointer has rested for `HOVER_DELAY` since its last move, sleeping
/// again for the rest each time a move pushed it back, and only then lands on the editor.
fn arm_hover_timer(moved_at: Arc<Mutex<Instant>>) {
    tokio::spawn(async move {
        loop {
            let rest = HOVER_DELAY.saturating_sub(lock(&moved_at).elapsed());
            if rest.is_zero() {
                break;
            }
            tokio::time::sleep(rest).await;
        }
        crate::job::dispatch(hover_at_pointer).await;
    });
}

/// The word or the line at a position, as a double or a triple click takes it.
/// The left button let go: what the mouse selected goes to the register a middle click
/// pastes from, where that is on.
fn mouse_selection_done(cxt: &mut commands::Context) -> EventResult {
    let config = cxt.editor.config();
    if !config.middle_click_paste {
        return EventResult::Ignored(None);
    }

    let (view, doc) = current!(cxt.editor);

    let should_yank = match cxt.editor.mouse_down_range.take() {
        Some(down_range) => doc.selection(view.id).primary() != down_range,
        None => {
            // This should not happen under normal cases. We fall back to the original
            // behavior of yanking on non-single-char selections.
            doc.selection(view.id)
                .primary()
                .slice(doc.text().slice(..))
                .len_chars()
                > 1
        }
    };

    if should_yank {
        commands::yank_main_selection_to_register(cxt.editor, config.mouse_yank_register);
        EventResult::Consumed(None)
    } else {
        EventResult::Ignored(None)
    }
}

/// A message cut into lines no wider than `width`, breaking between words where it can and
/// inside one only when a single word is wider than the box.
fn wrap_to_width(message: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in message.split_whitespace() {
        if line.is_empty() {
            line.push_str(word);
        } else if line.width() + 1 + word.width() <= width {
            line.push(' ');
            line.push_str(word);
        } else {
            lines.push(take(&mut line));
            line.push_str(word);
        }
        while line.width() > width {
            let mut cut = String::new();
            let mut rest = String::new();
            for ch in line.chars() {
                if cut.width() + ch.to_string().width() <= width {
                    cut.push(ch);
                } else {
                    rest.push(ch);
                }
            }
            lines.push(cut);
            line = rest;
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

fn unit_range(text: helix_core::RopeSlice, pos: usize, unit: ClickUnit) -> Range {
    match unit {
        ClickUnit::Word => textobject_word(text, Range::point(pos), TextObject::Inside, 1, false),
        ClickUnit::Line => {
            let line = text.char_to_line(pos);
            Range::new(text.line_to_char(line), text.line_to_char(line + 1))
        }
    }
}

/// The hover timer landing: the pointer has rested on a cell for a while. Asks what is
/// there if it is text the popup is not already open for, and closes the popup when the
/// pointer has left it.
fn hover_at_pointer(editor: &mut Editor, compositor: &mut Compositor) {
    let Some(view) = compositor.find::<EditorView>() else {
        return;
    };
    view.hover_armed = false;

    // Moved again between the timer and this landing: wait out the rest.
    if lock(&view.pointer_moved_at).elapsed() < HOVER_DELAY {
        view.hover_armed = true;
        arm_hover_timer(view.pointer_moved_at.clone());
        return;
    }
    let Some((row, column)) = view.pointer else {
        return;
    };
    let shown = view.hover_shown;

    // Reading the popup is not asking for another.
    let screen = compositor.size();
    let popup = compositor.find_id::<Popup<Hover>>(Hover::ID);
    let open = popup.is_some();
    if popup.is_some_and(|popup| hits(popup.area(screen, editor), row, column)) {
        return;
    }

    let under = editor.tree.views().find_map(|(view, _focus)| {
        let doc = &editor.documents[&view.doc];
        view.pos_at_screen_coords(doc, row, column, true)
            .map(|pos| (doc.id(), pos))
    });
    let Some((doc_id, pos)) = under else {
        compositor.remove(Hover::ID);
        return;
    };

    if open && shown == Some((doc_id, pos)) {
        return;
    }
    compositor.remove(Hover::ID);

    let doc = &editor.documents[&doc_id];
    let diagnostic = doc
        .diagnostics()
        .iter()
        .find(|diagnostic| diagnostic.range.start <= pos && pos < diagnostic.range.end)
        .map(|diagnostic| {
            let source = diagnostic
                .source
                .clone()
                .unwrap_or_else(|| "diagnostic".to_string());
            let hover = lsp::Hover {
                contents: lsp::HoverContents::Scalar(lsp::MarkedString::String(
                    diagnostic.message.clone(),
                )),
                range: None,
            };
            (source, hover)
        });
    let request = commands::lsp::request_hover(doc, pos);
    if diagnostic.is_none() && request.is_empty() {
        return;
    }

    let Some(view) = compositor.find::<EditorView>() else {
        return;
    };
    view.hover_shown = Some((doc_id, pos));

    let at = Position::new(row as usize, column as usize);
    tokio::spawn(async move {
        let mut hovers = request.answer().await;
        // What is wrong there comes first: it is what one points at to find out.
        if let Some(diagnostic) = diagnostic {
            hovers.insert(0, diagnostic);
        }
        crate::job::dispatch(move |editor, compositor| {
            let Some(view) = compositor.find::<EditorView>() else {
                return;
            };
            // The pointer has moved on since the question was asked.
            if view.hover_shown != Some((doc_id, pos)) {
                return;
            }
            if hovers.is_empty() {
                view.hover_shown = None;
                return;
            }
            commands::lsp::show_hover(editor, compositor, hovers, Some(at));
        })
        .await;
    });
}
