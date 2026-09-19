use std::{borrow::Cow, sync::Arc, time::Duration};

use helix_core::{
    syntax,
    unicode::width::{UnicodeWidthChar, UnicodeWidthStr},
    Rope, Selection,
};
use helix_stdx::Url;
use helix_view::{
    align_view, current, current_ref, doc,
    editor::Action,
    graphics::{Modifier, Rect, Style, UnderlineStyle},
    input::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind},
    keyboard::KeyModifiers,
    Align, Document, DocumentId, Editor, Theme,
};
use pulldown_cmark::{
    Alignment, BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd,
};
use tui::{
    buffer::Buffer as Surface,
    text::{Span, Spans},
};

use crate::{
    commands,
    compositor::EventResult,
    ui::{editor, markdown::highlighted_code_block, panel_width, sidebar::EDITOR_ROOM},
};

/// Under this many columns a preview is not worth drawing, and the file being edited
/// keeps the room.
const MIN_WIDTH: u16 = 30;

/// The widest the text is drawn at when the preview has the whole screen: a line running
/// the width of a terminal is not a line anybody reads.
const READING_WIDTH: u16 = 90;

/// Where the width the separator was dragged to is remembered.
const WIDTH_FILE: &str = "preview";

/// How long the file must stay unchanged before the preview is laid out again: a
/// keystroke is not worth reparsing the whole file for, a pause is.
const REST: Duration = Duration::from_millis(200);

/// The focused Markdown file drawn beside it as it reads: headings, lists, quotes,
/// tables and highlighted code, reflowed to the panel's width. It follows the file's
/// scroll unless the mouse wheel moved it, and then only until the file moves.
#[derive(Default)]
pub struct MarkdownPreview {
    pub open: bool,
    /// Whether the preview has the screen to itself, the file behind it.
    pub full: bool,
    /// Where it was drawn last, empty while it is not on screen.
    area: Rect,
    /// The width the separator was dragged to, kept between sessions over half the screen.
    width: Option<u16>,
    /// Whether the separator is being dragged, so the mouse is the preview's wherever it goes.
    resizing: bool,
    /// Where the text was drawn last, so a click can be turned back into a row.
    content: Rect,
    rendered: Option<Rendered>,
    scrolled: Option<Scrolled>,
    rest: Option<Rest>,
    /// What the mouse drew over the text, if anything is drawn over.
    selection: Option<PreviewSelection>,
    /// Whether the mouse is drawing one, so the drag is the preview's wherever the
    /// pointer goes, as the separator's is.
    selecting: bool,
}

/// What the mouse selected: where the button went down and where the pointer is, as a row
/// of the panel and a column in it. The rows are the ones laid out, not the ones on
/// screen, so scrolling the panel or the file leaves the selection on the same words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PreviewSelection {
    anchor: (usize, usize),
    head: (usize, usize),
}

impl PreviewSelection {
    /// Its ends in reading order, whichever way it was drawn.
    fn ends(&self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}

/// The rows last drawn, and what they were drawn from.
struct Rendered {
    doc: DocumentId,
    version: i32,
    width: u16,
    theme: String,
    rows: Vec<Row>,
}

/// A scroll the wheel gave the preview, good while the file's top line stays put.
struct Scrolled {
    doc: DocumentId,
    top_line: usize,
    offset: usize,
}

/// An edit the preview is waiting out: the rows on screen are older than the file, and a
/// timer says when the file has been still for long enough to lay it out again.
struct Rest {
    doc: DocumentId,
    version: i32,
    settled: bool,
}

/// One row of the panel.
#[derive(Debug)]
pub struct Row {
    spans: Spans<'static>,
    /// The line of the file this row starts on, 0-indexed.
    line: usize,
    blank: bool,
    /// The links on the row, by the columns they cover.
    links: Vec<Link>,
}

/// A link as drawn: the columns of its row it covers, and where it goes.
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    from: usize,
    to: usize,
    target: Arc<str>,
}

impl MarkdownPreview {
    /// A width file that cannot be read costs the remembered width, not the preview.
    pub fn new() -> Self {
        let width = match panel_width::load(WIDTH_FILE) {
            Ok(width) => width,
            Err(err) => {
                log::error!("Could not read the preview's width: {err:#}");
                None
            }
        };

        Self {
            width,
            ..Self::default()
        }
    }

    pub fn toggle(&mut self, editor: &mut Editor) {
        if self.open {
            self.open = false;
            return;
        }

        if !is_markdown(doc!(editor)) {
            editor.set_error("The preview is for Markdown files");
            return;
        }

        self.open = true;
        self.scrolled = None;
    }

    /// Show or hide the preview on its own, filling the screen.
    pub fn toggle_full(&mut self, editor: &mut Editor) {
        if self.full {
            self.full = false;
            return;
        }

        if !is_markdown(doc!(editor)) {
            editor.set_error("The preview is for Markdown files");
            return;
        }

        self.full = true;
        self.scrolled = None;
    }

    /// Whether the preview is the one thing on screen. The editor moving to a file that
    /// is not Markdown closes it: there is nothing left to draw.
    pub fn is_full(&mut self, editor: &Editor) -> bool {
        if self.full && !is_markdown(doc!(editor)) {
            self.full = false;
        }

        self.full
    }

    /// The columns the preview takes from the right of `area`: none while it is closed,
    /// the focused file is not Markdown, or what is left would not be worth drawing.
    pub fn width(&self, editor: &Editor, area: Rect) -> u16 {
        if !self.open || !is_markdown(doc!(editor)) {
            return 0;
        }

        let wanted = self.width.unwrap_or(area.width / 2);
        let width = wanted.min(area.width.saturating_sub(EDITOR_ROOM));
        if width < MIN_WIDTH {
            0
        } else {
            width
        }
    }

    /// Whether the separator is being dragged: the mouse is the preview's until it is
    /// let go, wherever it has gone.
    pub fn resizing(&self) -> bool {
        self.resizing
    }

    /// Whether the mouse is drawing a selection over the text: the drag stays the
    /// preview's when the pointer leaves it, so it can be grown past the panel's edge.
    pub fn selecting(&self) -> bool {
        self.selecting
    }

    /// Whether anything in the panel is selected, and so is what Copy copies.
    pub fn has_selection(&self) -> bool {
        self.selection
            .is_some_and(|selection| !selection.is_empty())
    }

    /// Nothing is selected any more. Answers whether anything was: the screen only needs
    /// drawing again if something was highlighted.
    pub fn clear_selection(&mut self) -> bool {
        self.selecting = false;
        self.selection.take().is_some()
    }

    /// What is selected, as the plain text it was drawn from: the rows it runs over, cut
    /// at the columns it starts and ends on, without the blanks each row is padded with.
    pub fn selected_text(&self) -> Option<String> {
        let selection = self.selection?;
        if selection.is_empty() {
            return None;
        }

        let rows = &self.rendered.as_ref()?.rows;
        let ((from_row, from_col), (to_row, to_col)) = selection.ends();
        let mut text = String::new();
        for index in from_row..=to_row.min(rows.len().saturating_sub(1)) {
            let row = rows.get(index)?;
            let from = if index == from_row { from_col } else { 0 };
            let to = if index == to_row { to_col } else { usize::MAX };
            if index != from_row {
                text.push('\n');
            }
            text.push_str(slice_columns(&row_text(row), from, to).trim_end());
        }

        (!text.trim().is_empty()).then_some(text)
    }

    /// Copy what is selected to the clipboard, the way Copy does over the file. Answers
    /// whether there was anything to copy.
    pub fn copy_selection(&self, editor: &mut Editor) -> bool {
        let Some(text) = self.selected_text() else {
            return false;
        };

        match editor.registers.write('+', vec![text]) {
            Ok(()) => editor.set_status("copied the preview's selection"),
            Err(err) => editor.set_error(err.to_string()),
        }

        true
    }

    /// Where the mouse leaves a selection is where it is yanked from, as in the file: the
    /// register the mouse yanks to is the system's primary selection on the machines that
    /// have one.
    fn yank_selection(&self, editor: &mut Editor) {
        let Some(text) = self.selected_text() else {
            return;
        };

        let register = editor.config().mouse_yank_register;
        if let Err(err) = editor.registers.write(register, vec![text]) {
            editor.set_error(err.to_string());
        }
    }

    /// The row and column of the text under the pointer, clamped to the panel: the
    /// pointer above it counts as the first row drawn and below it as the last, so a
    /// drag that leaves the panel keeps growing the selection.
    fn position_at(&self, editor: &Editor, row: u16, column: u16) -> Option<(usize, usize)> {
        let rendered = self.rendered.as_ref()?;
        if rendered.rows.is_empty() || self.content.height == 0 || self.content.width == 0 {
            return None;
        }

        let (view, doc) = current_ref!(editor);
        let offset = self.offset(doc.id(), top_line(doc, view.id));
        let on_row = row.clamp(self.content.y, self.content.bottom() - 1);
        let index = (offset + (on_row - self.content.y) as usize).min(rendered.rows.len() - 1);
        let column = column
            .saturating_sub(self.content.x)
            .min(self.content.width) as usize;

        Some((index, column))
    }

    /// Off the screen: the mouse no longer lands on it.
    pub fn hide(&mut self) {
        self.area = Rect::default();
    }

    pub fn contains(&self, row: u16, column: u16) -> bool {
        row >= self.area.y
            && row < self.area.bottom()
            && column >= self.area.x
            && column < self.area.right()
    }

    /// A key while the preview has the screen. It scrolls, and Escape closes it; anything
    /// else bare is swallowed, since the file is not on screen for a command to act on and
    /// a blind edit is the one thing a reading surface must not allow. A key with a
    /// modifier is one of the editor's shortcuts and goes on to the keymap.
    pub fn handle_key(&mut self, key: KeyEvent, editor: &Editor) -> EventResult {
        if !key.modifiers.is_empty() {
            return EventResult::Ignored(None);
        }

        let lines = editor.config().scroll_lines;
        let page = self.area.height.saturating_sub(2) as isize;

        match key.code {
            KeyCode::Esc => self.full = false,
            KeyCode::Down => self.scroll_by(editor, 1),
            KeyCode::Up => self.scroll_by(editor, -1),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_by(editor, page),
            KeyCode::PageUp => self.scroll_by(editor, -page),
            KeyCode::Home => self.scroll_by(editor, isize::MIN),
            KeyCode::End => self.scroll_by(editor, isize::MAX),
            KeyCode::Char('j') => self.scroll_by(editor, lines),
            KeyCode::Char('k') => self.scroll_by(editor, -lines),
            _ => {}
        }

        EventResult::Consumed(None)
    }

    /// The mouse over the preview. The terminal reports every motion of the pointer, and
    /// consuming one repaints the screen, so only an event that changed something is
    /// taken: the rest are ignored and cost nothing.
    pub fn handle_mouse(&mut self, event: &MouseEvent, cx: &mut commands::Context) -> EventResult {
        let lines = cx.editor.config().scroll_lines;

        match event.kind {
            // The separator is the preview's first column, so the panel follows the mouse
            // as it grows to the left.
            MouseEventKind::Down(MouseButton::Left)
                if !self.full && event.column == self.area.x =>
            {
                self.resizing = true;
            }
            MouseEventKind::Drag(MouseButton::Left) if self.resizing => {
                let total = self.area.width + cx.editor.tree.area().width;
                let most = total.saturating_sub(EDITOR_ROOM).max(MIN_WIDTH);
                let wanted = self.area.right().saturating_sub(event.column);
                self.width = Some(wanted.clamp(MIN_WIDTH, most));
            }
            MouseEventKind::Up(MouseButton::Left) if self.resizing => {
                self.resizing = false;
                let Some(width) = self.width else {
                    return EventResult::Consumed(None);
                };

                if let Err(err) = panel_width::save(WIDTH_FILE, width) {
                    log::error!("Could not remember the preview's width: {err:#}");
                    cx.editor
                        .set_error(format!("Could not remember the preview's width: {err:#}"));
                }
            }
            // A press starts a selection, and Shift takes the one already drawn to the
            // pointer, as they do over the file. What the press landed on is only decided
            // when the button is let go: a click follows a link, a drag selects.
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(position) = self.position_at(cx.editor, event.row, event.column) else {
                    return EventResult::Ignored(None);
                };

                self.selection = match self.selection {
                    Some(selection) if event.modifiers == KeyModifiers::SHIFT => {
                        Some(PreviewSelection {
                            anchor: selection.anchor,
                            head: position,
                        })
                    }
                    _ => Some(PreviewSelection {
                        anchor: position,
                        head: position,
                    }),
                };
                self.selecting = true;
            }
            MouseEventKind::Drag(MouseButton::Left) if self.selecting => {
                let Some(position) = self.position_at(cx.editor, event.row, event.column) else {
                    return EventResult::Ignored(None);
                };
                let Some(selection) = &mut self.selection else {
                    return EventResult::Ignored(None);
                };
                if selection.head == position {
                    return EventResult::Ignored(None);
                }

                selection.head = position;
            }
            MouseEventKind::Up(MouseButton::Left) if self.selecting => {
                self.selecting = false;

                // Down and up on the same spot is a click, and a click on a link follows it.
                if !self.has_selection() {
                    self.selection = None;
                    if let Some(target) = self.link_at(cx.editor, event.row, event.column) {
                        self.follow(&target, cx);
                    }
                    return EventResult::Consumed(None);
                }

                self.yank_selection(cx.editor);
            }
            MouseEventKind::ScrollDown => self.scroll_by(cx.editor, lines),
            MouseEventKind::ScrollUp => self.scroll_by(cx.editor, -lines),
            _ => return EventResult::Ignored(None),
        }

        EventResult::Consumed(None)
    }

    /// Where the link drawn under the pointer goes, if one is.
    fn link_at(&self, editor: &Editor, row: u16, column: u16) -> Option<Arc<str>> {
        let rendered = self.rendered.as_ref()?;
        let content = self.content;
        if row < content.y || row >= content.bottom() || column < content.x {
            return None;
        }

        let (view, doc) = current_ref!(editor);
        let offset = self.offset(doc.id(), top_line(doc, view.id));
        let drawn = rendered.rows.get(offset + (row - content.y) as usize)?;
        let at = (column - content.x) as usize;
        drawn
            .links
            .iter()
            .find(|link| link.from <= at && at < link.to)
            .map(|link| link.target.clone())
    }

    /// Follows a link: anything with a scheme goes to the system's opener, a path opens
    /// in the editor beside its file, and an anchor goes to the heading it names.
    fn follow(&mut self, target: &str, cx: &mut commands::Context) {
        if let Ok(url) = Url::parse(target) {
            cx.jobs.callback(crate::open_external_url_callback(url));
            return;
        }

        let (path, anchor) = match target.split_once('#') {
            Some((path, anchor)) => (path, Some(anchor)),
            None => (target, None),
        };

        if !path.is_empty() {
            let doc = doc!(cx.editor);
            let beside = doc
                .path()
                .and_then(|file| file.parent())
                .map_or_else(helix_stdx::env::current_working_dir, |dir| {
                    dir.to_path_buf()
                });
            let path = beside.join(path);
            if let Err(err) = cx.editor.open(&path, Action::Replace) {
                log::error!("Could not open {}: {err:#}", path.display());
                cx.editor
                    .set_error(format!("Could not open {}: {err:#}", path.display()));
                return;
            }
        }

        let Some(anchor) = anchor else {
            return;
        };

        let (view, doc) = current!(cx.editor);
        let Some(line) = heading_line(doc.text(), anchor) else {
            return;
        };

        let at = doc.text().line_to_char(line);
        doc.set_selection(view.id, Selection::point(at));
        align_view(doc, view, Align::Center);
    }

    fn scroll_by(&mut self, editor: &Editor, delta: isize) {
        let Some(rendered) = &self.rendered else {
            return;
        };

        let (view, doc) = current_ref!(editor);
        let top_line = top_line(doc, view.id);
        let offset = self
            .offset(doc.id(), top_line)
            .saturating_add_signed(delta)
            .min(last_offset(&rendered.rows, self.area.height));

        self.scrolled = Some(Scrolled {
            doc: doc.id(),
            top_line,
            offset,
        });
    }

    /// The first row on screen: the wheel's while the file has not moved, otherwise
    /// the row drawn from the file's top line.
    fn offset(&self, doc: DocumentId, top_line: usize) -> usize {
        let rows = match &self.rendered {
            Some(rendered) => &rendered.rows,
            None => return 0,
        };

        let offset = match &self.scrolled {
            Some(scrolled) if scrolled.doc == doc && scrolled.top_line == top_line => {
                scrolled.offset
            }
            _ => row_for_line(rows, top_line),
        };

        offset.min(last_offset(rows, self.area.height))
    }

    pub fn render(&mut self, area: Rect, surface: &mut Surface, editor: &Editor) {
        let theme = &editor.theme;
        surface.clear_with(area, theme.get("ui.background"));

        let separator = theme.get("ui.window");
        for y in area.y..area.bottom() {
            surface.set_string(area.x, y, "│", separator);
        }

        // A column of air on each side of the text.
        let content = area.clip_left(2).clip_right(1);
        self.draw(area, content, surface, editor);
    }

    /// The preview with the screen to itself: no separator, and the text in a column of
    /// its own width in the middle of it.
    pub fn render_full(&mut self, area: Rect, surface: &mut Surface, editor: &Editor) {
        surface.clear_with(area, editor.theme.get("ui.background"));

        let width = READING_WIDTH.min(area.width.saturating_sub(4));
        let margin = (area.width - width) / 2;
        let content = area.clip_left(margin).with_width(width);

        self.draw(area, content, surface, editor);
    }

    fn draw(&mut self, area: Rect, content: Rect, surface: &mut Surface, editor: &Editor) {
        self.area = area;
        self.content = content;

        let theme = &editor.theme;
        let (view, doc) = current_ref!(editor);
        if self.wants_layout(doc, content.width, theme) {
            self.rest = None;
            let text: Cow<str> = doc.text().slice(..).into();
            let rows = render_markdown(&text, content.width, theme, &editor.syn_loader.load());
            self.rendered = Some(Rendered {
                doc: doc.id(),
                version: doc.version(),
                width: content.width,
                theme: theme.name().to_string(),
                rows,
            });
        }

        let offset = self.offset(doc.id(), top_line(doc, view.id));
        let selection = self
            .selection
            .filter(|selection| !selection.is_empty())
            .map(|selection| selection.ends());
        let selected = theme.get("ui.selection");
        let rows = &self.rendered.as_ref().expect("rendered above").rows;
        for (y, (index, row)) in
            (content.y..content.bottom()).zip(rows.iter().enumerate().skip(offset))
        {
            surface.set_spans(content.x, y, &row.spans, content.width);

            // What the mouse selected, drawn over the text: a row inside it is taken from
            // its first column to the end of what is written on it.
            let Some(((from_row, from_col), (to_row, to_col))) = selection else {
                continue;
            };
            if index < from_row || index > to_row {
                continue;
            }
            let from = if index == from_row { from_col } else { 0 };
            let to = if index == to_row {
                to_col
            } else {
                row.spans.width()
            };
            let from = from.min(content.width as usize) as u16;
            let to = to.min(content.width as usize) as u16;
            if to > from {
                surface.set_style(Rect::new(content.x + from, y, to - from, 1), selected);
            }
        }
    }

    /// Whether the rows are laid out again on this draw. Another file, another width or
    /// another theme: at once. An edit of the same file: only once it has rested, the
    /// old rows staying on screen meanwhile, since every keystroke would otherwise
    /// reparse the whole file.
    fn wants_layout(&mut self, doc: &Document, width: u16, theme: &Theme) -> bool {
        let Some(rendered) = &self.rendered else {
            return true;
        };
        if rendered.doc != doc.id() || rendered.width != width || rendered.theme != theme.name() {
            return true;
        }
        if rendered.version == doc.version() {
            return false;
        }

        match &self.rest {
            Some(rest) if rest.doc == doc.id() && rest.version == doc.version() => rest.settled,
            _ => {
                let (id, version) = (doc.id(), doc.version());
                self.rest = Some(Rest {
                    doc: id,
                    version,
                    settled: false,
                });
                editor::later(REST, move |_, view| {
                    view.markdown_preview.rested(id, version);
                });

                false
            }
        }
    }

    /// The timer's word that the file stayed at `version` for long enough. A later edit
    /// armed a timer of its own, so an older one changes nothing.
    fn rested(&mut self, doc: DocumentId, version: i32) {
        if let Some(rest) = &mut self.rest {
            if rest.doc == doc && rest.version == version {
                rest.settled = true;
            }
        }
    }
}

/// The line of the heading a `#anchor` names, slugged the way GitHub does it.
fn heading_line(text: &Rope, anchor: &str) -> Option<usize> {
    let wanted = slug(anchor);
    text.lines().position(|line| {
        let line: Cow<str> = line.into();
        let heading = line.trim_start().strip_prefix('#');
        heading.is_some_and(|rest| slug(rest.trim_start_matches('#')) == wanted)
    })
}

/// A heading's anchor: lowercase, a hyphen per space, the punctuation gone.
fn slug(heading: &str) -> String {
    heading
        .trim()
        .chars()
        .filter(|ch| ch.is_alphanumeric() || *ch == ' ' || *ch == '-' || *ch == '_')
        .map(|ch| {
            if ch == ' ' {
                '-'
            } else {
                ch.to_ascii_lowercase()
            }
        })
        .collect()
}

fn is_markdown(doc: &Document) -> bool {
    doc.language_name() == Some("markdown")
}

/// The text of a row, without the styles it is drawn with.
fn row_text(row: &Row) -> String {
    row.spans
        .0
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// The part of `text` that is drawn between two columns.
fn slice_columns(text: &str, from: usize, to: usize) -> String {
    let mut column = 0;
    let mut cut = String::new();
    for ch in text.chars() {
        if column >= to {
            break;
        }
        let width = ch.width().unwrap_or(0);
        if column >= from && column + width <= to {
            cut.push(ch);
        }
        column += width;
    }

    cut
}

fn top_line(doc: &Document, view: helix_view::ViewId) -> usize {
    let anchor = doc.view_offset(view).anchor;
    doc.text().char_to_line(anchor.min(doc.text().len_chars()))
}

/// The furthest the rows can scroll and still fill `height`.
fn last_offset(rows: &[Row], height: u16) -> usize {
    rows.len().saturating_sub(height as usize)
}

/// The first row drawn from the block `line` belongs to.
fn row_for_line(rows: &[Row], line: usize) -> usize {
    let Some(target) = rows
        .iter()
        .map(|row| row.line)
        .filter(|start| *start <= line)
        .max()
    else {
        return 0;
    };

    rows.iter()
        .position(|row| row.line == target && !row.blank)
        .unwrap_or(0)
}

/// The theme's styles the preview draws with.
struct Styles {
    text: Style,
    headings: [Style; 6],
    code: Style,
    rule: Style,
    quote: Style,
    list: Style,
    link: Style,
    dim: Style,
}

impl Styles {
    fn new(theme: &Theme) -> Self {
        let heading = |level: usize| {
            theme
                .get(&format!("markup.heading.{level}"))
                .add_modifier(Modifier::BOLD)
        };

        Self {
            text: theme.get("ui.text"),
            headings: [
                heading(1),
                heading(2),
                heading(3),
                heading(4),
                heading(5),
                heading(6),
            ],
            code: theme.get("markup.raw.inline"),
            rule: theme.get("punctuation.special"),
            quote: theme.get("markup.quote"),
            list: theme.get("markup.list"),
            link: theme
                .get("markup.link.text")
                .underline_style(UnderlineStyle::Line),
            dim: theme.get("ui.text.inactive"),
        }
    }

    /// A GitHub alert (`> [!NOTE]`) wears the colour of the diagnostic it is closest to.
    fn alert(theme: &Theme, kind: BlockQuoteKind) -> (&'static str, Style) {
        let (label, scope) = match kind {
            BlockQuoteKind::Note => ("Note", "info"),
            BlockQuoteKind::Tip => ("Tip", "hint"),
            BlockQuoteKind::Important => ("Important", "special"),
            BlockQuoteKind::Warning => ("Warning", "warning"),
            BlockQuoteKind::Caution => ("Caution", "error"),
        };

        (label, theme.get(scope))
    }
}

/// A run of text as the parser handed it, with the style it is drawn in and the line
/// of the file it came from.
struct Piece {
    text: String,
    style: Style,
    line: usize,
    /// Where the text goes when it is a link.
    link: Option<Arc<str>>,
}

/// What wraps the text being drawn, outermost first. Each gives every row a prefix.
enum Container {
    Quote(Style),
    List(Option<u64>),
    /// A list item: its marker goes on its first row, spaces as wide on the rest.
    Item {
        marker: String,
        used: bool,
    },
}

struct Table {
    alignments: Vec<Alignment>,
    /// Each row's cells, and the line it starts on. The first is the header.
    rows: Vec<(Vec<Vec<Piece>>, usize)>,
    cells: Vec<Vec<Piece>>,
}

struct Renderer<'a> {
    width: usize,
    theme: &'a Theme,
    styles: Styles,
    loader: &'a syntax::Loader,
    line_starts: Vec<usize>,
    rows: Vec<Row>,
    containers: Vec<Container>,
    pieces: Vec<Piece>,
    /// The inline styles open around the text, innermost last.
    inline: Vec<Style>,
    /// The link the text being read is inside, if any.
    link: Option<Arc<str>>,
    /// The line the block being drawn starts on.
    block_line: usize,
    /// A code block being collected: its language and text.
    code: Option<(String, String)>,
    /// An HTML block being collected.
    html: Option<String>,
    table: Option<Table>,
}

/// Draws `text` as rows `width` columns wide.
pub fn render_markdown(text: &str, width: u16, theme: &Theme, loader: &syntax::Loader) -> Vec<Row> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_GFM);

    let line_starts = std::iter::once(0)
        .chain(text.match_indices('\n').map(|(at, _)| at + 1))
        .collect();

    let mut renderer = Renderer {
        width: (width as usize).max(1),
        theme,
        styles: Styles::new(theme),
        loader,
        line_starts,
        rows: Vec::new(),
        containers: Vec::new(),
        pieces: Vec::new(),
        inline: Vec::new(),
        link: None,
        block_line: 0,
        code: None,
        html: None,
        table: None,
    };

    for (event, range) in Parser::new_ext(text, options).into_offset_iter() {
        renderer.event(event, range.start);
    }
    renderer.flush();

    while renderer.rows.last().is_some_and(|row| row.blank) {
        renderer.rows.pop();
    }

    renderer.rows
}

impl Renderer<'_> {
    fn line_of(&self, offset: usize) -> usize {
        self.line_starts
            .partition_point(|start| *start <= offset)
            .saturating_sub(1)
    }

    fn style(&self) -> Style {
        self.inline
            .iter()
            .fold(self.styles.text, |style, inline| style.patch(*inline))
    }

    fn push_text(&mut self, text: &str, style: Style, offset: usize) {
        let line = self.line_of(offset);
        self.pieces.push(Piece {
            text: text.to_string(),
            style,
            line,
            link: self.link.clone(),
        });
    }

    fn event(&mut self, event: Event, offset: usize) {
        match event {
            Event::Start(tag) => self.start(tag, offset),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => {
                if let Some((_, code)) = &mut self.code {
                    code.push_str(&text);
                } else {
                    let style = self.style();
                    self.push_text(&text, style, offset);
                }
            }
            Event::Html(html) => {
                if let Some(block) = &mut self.html {
                    block.push_str(&html);
                }
            }
            // The text between inline tags comes as text; of the tags, only a break and
            // an image say something.
            Event::InlineHtml(html) => {
                for part in read_html(&html) {
                    match part {
                        HtmlPart::Break => self.flush(),
                        HtmlPart::Image(alt) => self.push_image(&alt, offset),
                        HtmlPart::Text(_) => {}
                    }
                }
            }
            Event::Code(text) | Event::InlineMath(text) | Event::DisplayMath(text) => {
                let style = self.style().patch(self.styles.code);
                self.push_text(&text, style, offset);
            }
            Event::FootnoteReference(label) => {
                let style = self.styles.link;
                self.push_text(&format!("[{label}]"), style, offset);
            }
            // Text reflows to the panel, so a line break in the file is a space.
            Event::SoftBreak => {
                let style = self.style();
                self.push_text(" ", style, offset);
            }
            Event::HardBreak => self.flush(),
            Event::Rule => {
                self.flush();
                self.block_line = self.line_of(offset);
                let rule = "─".repeat(self.room());
                let style = self.styles.rule;
                self.push_row(vec![Span::styled(rule, style)]);
                self.blank();
            }
            Event::TaskListMarker(checked) => {
                let marker = if checked { "☑ " } else { "☐ " };
                if let Some(Container::Item { marker: item, .. }) = self.containers.last_mut() {
                    *item = marker.to_string();
                }
            }
        }
    }

    fn start(&mut self, tag: Tag, offset: usize) {
        match tag {
            Tag::Paragraph => self.block_line = self.line_of(offset),
            Tag::Heading { level, .. } => {
                self.flush();
                self.block_line = self.line_of(offset);
                let style = self.styles.headings[heading_index(level)];
                self.inline.push(style);
            }
            Tag::BlockQuote(kind) => {
                self.flush();
                self.block_line = self.line_of(offset);
                match kind {
                    Some(kind) => {
                        let (label, style) = Styles::alert(self.theme, kind);
                        self.containers.push(Container::Quote(style));
                        let bold = style.add_modifier(Modifier::BOLD);
                        self.push_row(vec![Span::styled(label, bold)]);
                    }
                    None => self.containers.push(Container::Quote(self.styles.quote)),
                }
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                // A fenced block's first line is the fence.
                let (language, fence) = match kind {
                    CodeBlockKind::Fenced(language) => (language.to_string(), 1),
                    CodeBlockKind::Indented => (String::new(), 0),
                };
                self.block_line = self.line_of(offset) + fence;
                self.code = Some((language, String::new()));
            }
            Tag::HtmlBlock => {
                self.flush();
                self.block_line = self.line_of(offset);
                self.html = Some(String::new());
            }
            Tag::List(start) => {
                // The text of the item this list sits in goes on its own rows first.
                self.flush();
                self.containers.push(Container::List(start));
            }
            Tag::Item => {
                self.flush();
                self.block_line = self.line_of(offset);
                let depth = self
                    .containers
                    .iter()
                    .filter(|container| matches!(container, Container::List(_)))
                    .count();
                let marker = match self.containers.last_mut() {
                    Some(Container::List(Some(number))) => {
                        let marker = format!("{number}. ");
                        *number += 1;
                        marker
                    }
                    _ => {
                        let bullets = ["• ", "◦ ", "▪ "];
                        bullets[depth.saturating_sub(1) % bullets.len()].to_string()
                    }
                };
                self.containers.push(Container::Item {
                    marker,
                    used: false,
                });
            }
            Tag::FootnoteDefinition(label) => {
                self.flush();
                self.block_line = self.line_of(offset);
                let style = self.styles.link;
                self.push_text(&format!("[{label}] "), style, offset);
            }
            Tag::Table(alignments) => {
                self.flush();
                self.table = Some(Table {
                    alignments,
                    rows: Vec::new(),
                    cells: Vec::new(),
                });
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.cells.clear();
                }
                self.block_line = self.line_of(offset);
            }
            Tag::TableCell => self.pieces.clear(),
            Tag::Emphasis => self
                .inline
                .push(Style::default().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self
                .inline
                .push(Style::default().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => {
                let style = Style::default().add_modifier(Modifier::CROSSED_OUT);
                self.inline.push(style);
            }
            Tag::Link { dest_url, .. } => {
                self.link = Some(Arc::from(dest_url.as_ref()));
                self.inline.push(self.styles.link);
            }
            Tag::Image { .. } => {
                let style = self.styles.dim;
                self.push_text("[image: ", style, offset);
                self.inline.push(style);
            }
            Tag::Superscript
            | Tag::Subscript
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::MetadataBlock(_) => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush();
                self.blank();
            }
            TagEnd::Heading(level) => {
                self.inline.pop();
                self.flush();
                let underline = match level {
                    HeadingLevel::H1 => Some(("━", self.styles.headings[0])),
                    HeadingLevel::H2 => Some(("─", self.styles.rule)),
                    _ => None,
                };
                if let Some((stroke, style)) = underline {
                    let rule = stroke.repeat(self.room());
                    self.push_row(vec![Span::styled(rule, style)]);
                }
                self.blank();
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.close_quote();
                self.blank();
            }
            // HTML is drawn as the page would show it: its text and its images, no tags.
            TagEnd::HtmlBlock => {
                let html = self.html.take().unwrap_or_default();
                let offset = self.line_starts[self.block_line];
                for part in read_html(&html) {
                    match part {
                        HtmlPart::Text(text) => {
                            let style = self.styles.text;
                            self.push_text(&text, style, offset);
                        }
                        HtmlPart::Image(alt) => self.push_image(&alt, offset),
                        HtmlPart::Break => self.flush(),
                    }
                }
                self.flush();
                self.blank();
            }
            TagEnd::CodeBlock => {
                let Some((language, code)) = self.code.take() else {
                    return;
                };
                let lines = highlighted_code_block(
                    code.trim_end_matches('\n'),
                    &language,
                    Some(self.theme),
                    self.loader,
                    None,
                );
                let first = self.block_line;
                for (index, line) in lines.lines.into_iter().enumerate() {
                    self.block_line = first + index;
                    let mut spans = vec![Span::raw("  ")];
                    spans.extend(line.0.into_iter().map(own_span));
                    self.push_row(spans);
                }
                self.blank();
            }
            TagEnd::List(_) => {
                self.flush();
                self.containers.pop();
                let nested = self
                    .containers
                    .iter()
                    .any(|container| matches!(container, Container::List(_)));
                if !nested {
                    self.blank();
                }
            }
            TagEnd::Item => {
                self.flush();
                self.containers.pop();
            }
            TagEnd::FootnoteDefinition => {
                self.flush();
                self.blank();
            }
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.pieces);
                if let Some(table) = &mut self.table {
                    table.cells.push(cell);
                }
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                let line = self.block_line;
                if let Some(table) = &mut self.table {
                    let cells = std::mem::take(&mut table.cells);
                    table.rows.push((cells, line));
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.draw_table(table);
                }
                self.blank();
            }
            TagEnd::Image => {
                self.inline.pop();
                let line = self
                    .pieces
                    .last()
                    .map_or(self.block_line, |piece| piece.line);
                self.pieces.push(Piece {
                    text: "]".to_string(),
                    style: self.styles.dim,
                    line,
                    link: self.link.clone(),
                });
            }
            TagEnd::Link => {
                self.inline.pop();
                self.link = None;
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.inline.pop();
            }
            TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::MetadataBlock(_) => {}
        }
    }
}

impl Renderer<'_> {
    fn push_image(&mut self, alt: &str, offset: usize) {
        let text = if alt.is_empty() {
            "[image]".to_string()
        } else {
            format!("[image: {alt}]")
        };
        let style = self.styles.dim;
        self.push_text(&text, style, offset);
    }

    /// The columns left for text once the containers' prefixes are drawn.
    fn room(&self) -> usize {
        let prefix: usize = self
            .containers
            .iter()
            .map(|container| match container {
                Container::Quote(_) => 2,
                Container::List(_) => 0,
                Container::Item { marker, .. } => marker.width(),
            })
            .sum();

        self.width.saturating_sub(prefix).max(1)
    }

    /// What goes in front of a row: a bar per quote, and per list item its marker on
    /// the item's first row and as many spaces on the rest.
    fn prefix(&mut self) -> Vec<Span<'static>> {
        let mut spans = Vec::new();
        for container in &mut self.containers {
            match container {
                Container::Quote(style) => spans.push(Span::styled("▎ ", *style)),
                Container::List(_) => {}
                Container::Item { marker, used: true } => {
                    spans.push(Span::raw(" ".repeat(marker.width())));
                }
                Container::Item { marker, used } => {
                    spans.push(Span::styled(marker.clone(), self.styles.list));
                    *used = true;
                }
            }
        }

        spans
    }

    fn push_row(&mut self, spans: Vec<Span<'static>>) {
        let line = self.block_line;
        self.push_row_at(spans, line);
    }

    fn push_row_at(&mut self, spans: Vec<Span<'static>>, line: usize) {
        self.push_row_linked(spans, Vec::new(), line);
    }

    /// A row with links on it, their columns counted from the text's start: the prefix
    /// drawn in front moves them along.
    fn push_row_linked(&mut self, spans: Vec<Span<'static>>, links: Vec<Link>, line: usize) {
        let mut row = self.prefix();
        let shift: usize = row.iter().map(Span::width).sum();
        row.extend(spans);
        let links = links
            .into_iter()
            .map(|link| Link {
                from: link.from + shift,
                to: link.to + shift,
                target: link.target,
            })
            .collect();
        self.rows.push(Row {
            spans: Spans(row),
            line,
            blank: false,
            links,
        });
    }

    /// An empty row between blocks: one at most, never the first, and inside a quote it
    /// keeps the quote's bar.
    fn blank(&mut self) {
        let Some(last) = self.rows.last() else {
            return;
        };
        if last.blank {
            return;
        }

        let line = last.line;
        let spans = self
            .containers
            .iter()
            .filter_map(|container| match container {
                Container::Quote(style) => Some(Span::styled("▎ ", *style)),
                _ => None,
            })
            .collect();
        self.rows.push(Row {
            spans: Spans(spans),
            line,
            blank: true,
            links: Vec::new(),
        });
    }

    /// Draws the text collected so far, wrapped to the room the containers leave.
    fn flush(&mut self) {
        if self.pieces.is_empty() {
            return;
        }

        let pieces = std::mem::take(&mut self.pieces);
        let room = self.room();
        for wrapped in wrap(&pieces, room, self.styles.text) {
            self.push_row_linked(wrapped.spans, wrapped.links, wrapped.line);
        }
    }

    fn close_quote(&mut self) {
        // The quote's bar stops at its last line of text.
        if self.rows.last().is_some_and(|row| row.blank) {
            self.rows.pop();
        }
        self.containers.pop();
    }

    fn draw_table(&mut self, table: Table) {
        let columns = table
            .rows
            .iter()
            .map(|(cells, _)| cells.len())
            .max()
            .unwrap_or(0);
        if columns == 0 {
            return;
        }

        let mut widths = vec![0; columns];
        for (cells, _) in &table.rows {
            for (column, cell) in cells.iter().enumerate() {
                widths[column] = widths[column].max(pieces_width(cell));
            }
        }

        // Too wide for the panel: the widest column gives a column at a time.
        let gaps = 3 * (columns - 1);
        let room = self.room();
        while widths.iter().sum::<usize>() + gaps > room {
            let (widest, width) = widths
                .iter()
                .copied()
                .enumerate()
                .max_by_key(|(_, width)| *width)
                .expect("a table has a column");
            if width <= 1 {
                break;
            }
            widths[widest] -= 1;
        }

        let border = self.styles.rule;
        for (index, (cells, line)) in table.rows.iter().enumerate() {
            let header = index == 0;
            let mut spans = Vec::new();
            for (column, width) in widths.iter().enumerate() {
                if column > 0 {
                    spans.push(Span::styled(" │ ", border));
                }
                let cell = cells.get(column).map_or(&[][..], Vec::as_slice);
                let alignment = table
                    .alignments
                    .get(column)
                    .copied()
                    .unwrap_or(Alignment::None);
                spans.extend(fit_cell(cell, *width, alignment, header));
            }
            self.push_row_at(spans, *line);

            if header {
                let rule: Vec<String> = widths.iter().map(|width| "─".repeat(*width)).collect();
                self.push_row_at(vec![Span::styled(rule.join("─┼─"), border)], *line);
            }
        }
    }
}

fn heading_index(level: HeadingLevel) -> usize {
    match level {
        HeadingLevel::H1 => 0,
        HeadingLevel::H2 => 1,
        HeadingLevel::H3 => 2,
        HeadingLevel::H4 => 3,
        HeadingLevel::H5 => 4,
        HeadingLevel::H6 => 5,
    }
}

fn own_span(span: Span<'_>) -> Span<'static> {
    Span::styled(span.content.into_owned(), span.style)
}

fn pieces_width(pieces: &[Piece]) -> usize {
    pieces.iter().map(|piece| piece.text.width()).sum()
}

/// A cell's text in exactly `width` columns: padded as the column aligns, or cut with
/// an ellipsis when it does not fit.
fn fit_cell(
    pieces: &[Piece],
    width: usize,
    alignment: Alignment,
    header: bool,
) -> Vec<Span<'static>> {
    let bold = |style: Style| {
        if header {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        }
    };

    let mut spans = Vec::new();
    let mut used = 0;
    let fits = pieces_width(pieces) <= width;
    'pieces: for piece in pieces {
        let mut text = String::new();
        for ch in piece.text.chars() {
            let ch_width = ch.width().unwrap_or(0);
            let limit = if fits { width } else { width.saturating_sub(1) };
            if used + ch_width > limit {
                // "a very…", never "a very …".
                let kept = text.trim_end().len();
                used -= text[kept..].width();
                text.truncate(kept);
                if !text.is_empty() {
                    spans.push(Span::styled(text, bold(piece.style)));
                }
                spans.push(Span::styled("…", bold(piece.style)));
                used += 1;
                break 'pieces;
            }
            text.push(ch);
            used += ch_width;
        }
        if !text.is_empty() {
            spans.push(Span::styled(text, bold(piece.style)));
        }
    }

    let pad = width.saturating_sub(used);
    let (left, right) = match alignment {
        Alignment::Right => (pad, 0),
        Alignment::Center => (pad / 2, pad - pad / 2),
        Alignment::Left | Alignment::None => (0, pad),
    };
    if left > 0 {
        spans.insert(0, Span::raw(" ".repeat(left)));
    }
    if right > 0 {
        spans.push(Span::raw(" ".repeat(right)));
    }

    spans
}

/// A run of one style inside a word or a row, and the link it is part of.
#[derive(Clone)]
struct Run {
    text: String,
    style: Style,
    link: Option<Arc<str>>,
}

impl Run {
    fn same_as(&self, style: Style, link: &Option<Arc<str>>) -> bool {
        self.style == style && self.link == *link
    }
}

/// A word to wrap: its styled runs, its width and where in the file it starts.
struct Word {
    runs: Vec<Run>,
    width: usize,
    line: usize,
    space_before: bool,
}

fn words(pieces: &[Piece]) -> Vec<Word> {
    let mut words = Vec::new();
    let mut current: Option<Word> = None;
    let mut space = false;

    for piece in pieces {
        for ch in piece.text.chars() {
            if ch.is_whitespace() {
                words.extend(current.take());
                space = true;
                continue;
            }

            let word = current.get_or_insert_with(|| Word {
                runs: Vec::new(),
                width: 0,
                line: piece.line,
                space_before: space,
            });
            space = false;
            match word.runs.last_mut() {
                Some(run) if run.same_as(piece.style, &piece.link) => run.text.push(ch),
                _ => word.runs.push(Run {
                    text: ch.to_string(),
                    style: piece.style,
                    link: piece.link.clone(),
                }),
            }
            word.width += ch.width().unwrap_or(0);
        }
    }
    words.extend(current);

    words
}

/// A row of wrapped text: its spans, the line of the file its first word is on, and the
/// links on it by the columns they cover.
struct Wrapped {
    spans: Vec<Span<'static>>,
    line: usize,
    links: Vec<Link>,
}

/// Turns the runs of a row into its spans and its links, the links by column.
fn finish_row(runs: Vec<Run>, line: usize) -> Wrapped {
    let mut spans = Vec::new();
    let mut links: Vec<Link> = Vec::new();
    let mut column = 0;
    for run in runs {
        let width = run.text.width();
        if let Some(target) = run.link {
            match links.last_mut() {
                Some(last) if last.to == column && last.target == target => last.to += width,
                _ => links.push(Link {
                    from: column,
                    to: column + width,
                    target,
                }),
            }
        }
        column += width;
        spans.push(Span::styled(run.text, run.style));
    }

    Wrapped { spans, line, links }
}

/// Breaks the pieces into rows no wider than `room`, between words where it can and
/// through a word only when the word alone is wider than a row.
fn wrap(pieces: &[Piece], room: usize, text_style: Style) -> Vec<Wrapped> {
    let mut rows = Vec::new();
    let mut row: Vec<Run> = Vec::new();
    let mut width = 0;
    let mut line = 0;

    for word in words(pieces) {
        let spaced = word.space_before && !row.is_empty();
        if !row.is_empty() && width + usize::from(spaced) + word.width > room {
            rows.push(finish_row(std::mem::take(&mut row), line));
            width = 0;
        }

        if row.is_empty() {
            line = word.line;
        } else if spaced {
            // A space between two runs of one style wears it, so a link stays underlined;
            // and between two runs of one link it is the link's, so the link stays one.
            let (before, after) = (row.last(), word.runs.first());
            let style = match (before, after) {
                (Some(before), Some(after)) if before.style == after.style => before.style,
                _ => text_style,
            };
            let link = match (before, after) {
                (Some(before), Some(after)) if before.link == after.link => before.link.clone(),
                _ => None,
            };
            row.push(Run {
                text: " ".to_string(),
                style,
                link,
            });
            width += 1;
        }

        for run in word.runs {
            for ch in run.text.chars() {
                let ch_width = ch.width().unwrap_or(0);
                if !row.is_empty() && width + ch_width > room {
                    rows.push(finish_row(std::mem::take(&mut row), line));
                    width = 0;
                    line = word.line;
                }
                match row.last_mut() {
                    Some(last) if last.same_as(run.style, &run.link) => last.text.push(ch),
                    _ => row.push(Run {
                        text: ch.to_string(),
                        style: run.style,
                        link: run.link.clone(),
                    }),
                }
                width += ch_width;
            }
        }
    }

    if !row.is_empty() {
        rows.push(finish_row(row, line));
    }

    rows
}

/// What a piece of HTML shows once its tags are gone.
#[derive(Debug, PartialEq)]
enum HtmlPart {
    Text(String),
    Image(String),
    Break,
}

/// Reads HTML the little a preview needs: the text between tags, the alt of each image
/// and where a `<br>` breaks the line. Comments and every other tag are dropped.
fn read_html(html: &str) -> Vec<HtmlPart> {
    let mut parts = Vec::new();
    let mut rest = html;

    while let Some(open) = rest.find('<') {
        let text = decode_entities(&rest[..open]);
        if !text.trim().is_empty() {
            parts.push(HtmlPart::Text(text));
        }

        let tag = &rest[open..];
        if let Some(comment) = tag.strip_prefix("<!--") {
            rest = comment.find("-->").map_or("", |end| &comment[end + 3..]);
            continue;
        }

        let Some(close) = tag.find('>') else {
            rest = "";
            break;
        };
        let inside = &tag[1..close];
        let name: String = inside
            .chars()
            .take_while(|ch| ch.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        match name.as_str() {
            "br" => parts.push(HtmlPart::Break),
            "img" => parts.push(HtmlPart::Image(html_attribute(inside, "alt"))),
            _ => {}
        }
        rest = &tag[close + 1..];
    }

    let text = decode_entities(rest);
    if !text.trim().is_empty() {
        parts.push(HtmlPart::Text(text));
    }

    parts
}

/// The value of `name="…"` (or with single quotes) inside a tag, empty when absent.
fn html_attribute(tag: &str, name: &str) -> String {
    for quote in ['"', '\''] {
        let key = format!("{name}={quote}");
        if let Some(start) = tag.find(&key) {
            let value = &tag[start + key.len()..];
            let end = value.find(quote).unwrap_or(value.len());
            return decode_entities(&value[..end]);
        }
    }

    String::new()
}

fn decode_entities(text: &str) -> String {
    text.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_core::syntax::config::Configuration;
    use std::collections::HashMap;

    fn rows(text: &str, width: u16) -> Vec<Row> {
        let loader = syntax::Loader::new(Configuration {
            language: Vec::new(),
            language_server: HashMap::new(),
        })
        .unwrap();

        render_markdown(text, width, &Theme::default(), &loader)
    }

    fn draw(text: &str, width: u16) -> Vec<String> {
        rows(text, width)
            .iter()
            .map(|row| String::from(&row.spans).trim_end().to_string())
            .collect()
    }

    #[test]
    fn a_paragraph_reflows_to_the_panel() {
        let text = "one two\nthree four five six\n";

        assert_eq!(draw(text, 12), ["one two", "three four", "five six"]);
    }

    #[test]
    fn a_word_wider_than_the_panel_is_cut_through() {
        assert_eq!(draw("abcdefghij", 4), ["abcd", "efgh", "ij"]);
    }

    #[test]
    fn blocks_are_one_blank_row_apart_and_the_big_headings_are_underlined() {
        let text = "# Title\n\nSome text.\n\n## Part\n\nMore.\n\n### Small\n";

        assert_eq!(
            draw(text, 8),
            [
                "Title",
                "━━━━━━━━",
                "",
                "Some",
                "text.",
                "",
                "Part",
                "────────",
                "",
                "More.",
                "",
                "Small"
            ],
        );
    }

    #[test]
    fn a_list_item_wraps_under_its_own_text() {
        let text = "- first item here\n- second\n  - nested one\n1. one\n2. two\n";

        assert_eq!(
            draw(text, 12),
            [
                "• first item",
                "  here",
                "• second",
                "  ◦ nested",
                "    one",
                "",
                "1. one",
                "2. two",
            ],
        );
    }

    #[test]
    fn a_task_shows_whether_it_is_done() {
        assert_eq!(draw("- [x] done\n- [ ] to do\n", 20), ["☑ done", "☐ to do"]);
    }

    #[test]
    fn a_quote_keeps_its_bar_on_every_row_and_no_further() {
        let text = "> quoted words wrap\n>\n> again\n\nafter\n";

        assert_eq!(
            draw(text, 10),
            ["▎ quoted", "▎ words", "▎ wrap", "▎", "▎ again", "", "after"],
        );
    }

    #[test]
    fn an_alert_says_what_it_is() {
        assert_eq!(
            draw("> [!WARNING]\n> Careful.\n", 20),
            ["▎ Warning", "▎ Careful."]
        );
    }

    #[test]
    fn a_table_lines_its_columns_up() {
        let text = "| Name | Age |\n|------|----:|\n| alice | 30 |\n| bob | 7 |\n";

        assert_eq!(
            draw(text, 40),
            ["Name  │ Age", "──────┼────", "alice │  30", "bob   │   7"],
        );
    }

    #[test]
    fn a_table_too_wide_gives_from_its_widest_column() {
        let text = "| a | b |\n|---|---|\n| short | a very long cell |\n";

        assert_eq!(
            draw(text, 16),
            ["a     │ b", "──────┼─────────", "short │ a very…"],
        );
    }

    #[test]
    fn code_keeps_its_lines_and_is_indented() {
        let text = "```\nlet a = 1;\n  let b = 2;\n```\n";

        assert_eq!(draw(text, 40), ["  let a = 1;", "    let b = 2;"]);
    }

    #[test]
    fn every_row_knows_the_line_it_came_from() {
        let text = "# Title\n\nfirst paragraph\ncontinues here\n\n```\ncode\n```\n\n- item\n";
        let rows = rows(text, 10);
        let lines: Vec<(String, usize)> = rows
            .iter()
            .filter(|row| !row.blank)
            .map(|row| (String::from(&row.spans).trim_end().to_string(), row.line))
            .collect();

        assert_eq!(
            lines,
            [
                ("Title".to_string(), 0),
                ("━━━━━━━━━━".to_string(), 0),
                ("first".to_string(), 2),
                ("paragraph".to_string(), 2),
                ("continues".to_string(), 3),
                ("here".to_string(), 3),
                ("  code".to_string(), 6),
                ("• item".to_string(), 9),
            ],
        );
    }

    #[test]
    fn the_preview_scrolls_to_the_block_at_the_files_top_line() {
        let text = "# Title\n\nfirst paragraph\ncontinues here\n\nsecond\n";
        let rows = rows(text, 10);
        let at = |line| String::from(&rows[row_for_line(&rows, line)].spans);

        assert_eq!(at(0), "Title");
        // A blank line between blocks belongs to the block above it.
        assert_eq!(at(1), "Title");
        assert_eq!(at(3), "continues");
        assert_eq!(at(5), "second");
        assert_eq!(at(99), "second");
    }

    fn links(text: &str, width: u16) -> Vec<Vec<(usize, usize, String)>> {
        rows(text, width)
            .iter()
            .map(|row| {
                row.links
                    .iter()
                    .map(|link| (link.from, link.to, link.target.to_string()))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn a_link_knows_the_columns_it_covers() {
        let text = "see [the docs](docs/x.md) now\n";

        assert_eq!(draw(text, 40), ["see the docs now"]);
        assert_eq!(links(text, 40), [vec![(4, 12, "docs/x.md".to_string())]]);
    }

    #[test]
    fn a_link_wrapped_over_rows_is_on_each_of_them() {
        let text = "[a long link text](x) tail\n";

        assert_eq!(draw(text, 8), ["a long", "link", "text", "tail"]);
        assert_eq!(
            links(text, 8),
            [
                vec![(0, 6, "x".to_string())],
                vec![(0, 4, "x".to_string())],
                vec![(0, 4, "x".to_string())],
                vec![],
            ],
        );
    }

    #[test]
    fn a_link_moves_past_the_prefix_and_bold_inside_it_is_still_one_link() {
        let text = "- [a **b** c](t)\n";

        assert_eq!(draw(text, 20), ["• a b c"]);
        assert_eq!(links(text, 20), [vec![(2, 7, "t".to_string())]]);
    }

    #[test]
    fn an_anchor_finds_its_heading_the_way_github_names_it() {
        let text = Rope::from("# sid\n\ntext\n\n## Tabs, splits and the mouse\n\n### C++ & Rust\n");

        assert_eq!(heading_line(&text, "tabs-splits-and-the-mouse"), Some(4));
        assert_eq!(heading_line(&text, "sid"), Some(0));
        assert_eq!(heading_line(&text, "c--rust"), Some(6));
        assert_eq!(heading_line(&text, "missing"), None);
    }

    #[test]
    fn html_shows_its_text_and_images_not_its_tags() {
        let text = "<div align=\"center\">\n\n<img alt=\"Logo\" src=\"x.svg\">\n\n# sid\n\n\
                    </div>\n\n<!-- a note -->\n\n<p>Tom &amp; Jerry</p>\n\nA<br>B\n";

        assert_eq!(
            draw(text, 20),
            [
                "[image: Logo]",
                "",
                "sid",
                "━━━━━━━━━━━━━━━━━━━━",
                "",
                "Tom & Jerry",
                "",
                "A",
                "B"
            ],
        );
    }
}
