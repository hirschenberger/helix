use crate::{
    compositor::{Component, Compositor, Context, EventResult},
    ui::EditorView,
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use tui::{
    buffer::Buffer as Surface,
    widgets::{Block, BorderType, Borders},
};

use fuzzy_matcher::skim::SkimMatcherV2 as Matcher;
use fuzzy_matcher::FuzzyMatcher;
use tui::widgets::Widget;

use std::{borrow::Cow, collections::HashMap, path::PathBuf};

use crate::ui::{Prompt, PromptEvent};
use helix_core::Position;
use helix_view::{
    document::canonicalize_path,
    editor::Action,
    graphics::{Color, CursorKind, Rect, Style},
    Document, Editor,
};

pub const MIN_SCREEN_WIDTH_FOR_PREVIEW: u16 = 80;

/// File path and line number (used to align and highlight a line)
type FileLocation = (PathBuf, Option<usize>);

pub struct FilePicker<T> {
    picker: Picker<T>,
    /// Caches paths to documents
    preview_cache: HashMap<PathBuf, Document>,
    /// Given an item in the picker, return the file path and line number to display.
    file_fn: Box<dyn Fn(&Editor, &T) -> Option<FileLocation>>,
}

impl<T> FilePicker<T> {
    pub fn new(
        options: Vec<T>,
        format_fn: impl Fn(&T) -> Cow<str> + 'static,
        callback_fn: impl Fn(&mut Editor, &T, Action) + 'static,
        preview_fn: impl Fn(&Editor, &T) -> Option<FileLocation> + 'static,
    ) -> Self {
        Self {
            picker: Picker::new(false, options, format_fn, callback_fn),
            preview_cache: HashMap::new(),
            file_fn: Box::new(preview_fn),
        }
    }

    fn current_file(&self, editor: &Editor) -> Option<FileLocation> {
        self.picker
            .selection()
            .and_then(|current| (self.file_fn)(editor, current))
            .and_then(|(path, line)| canonicalize_path(&path).ok().zip(Some(line)))
    }

    fn calculate_preview(&mut self, editor: &Editor) {
        if let Some((path, _line)) = self.current_file(editor) {
            if !self.preview_cache.contains_key(&path) && editor.document_by_path(&path).is_none() {
                // TODO: enable syntax highlighting; blocked by async rendering
                let doc = Document::open(&path, None, Some(&editor.theme), None).unwrap();
                self.preview_cache.insert(path, doc);
            }
        }
    }
}

impl<T: 'static> Component for FilePicker<T> {
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        // +---------+ +---------+
        // |prompt   | |preview  |
        // +---------+ |         |
        // |picker   | |         |
        // |         | |         |
        // +---------+ +---------+
        self.calculate_preview(cx.editor);
        let render_preview = area.width > MIN_SCREEN_WIDTH_FOR_PREVIEW;
        let area = inner_rect(area);
        // -- Render the frame:
        // clear area
        let background = cx.editor.theme.get("ui.background");
        surface.clear_with(area, background);

        let picker_width = if render_preview {
            area.width / 2
        } else {
            area.width
        };

        let picker_area = Rect::new(area.x, area.y, picker_width, area.height);
        self.picker.render(picker_area, surface, cx);

        if !render_preview {
            return;
        }

        let preview_area = Rect::new(area.x + picker_width, area.y, area.width / 2, area.height);

        // don't like this but the lifetime sucks
        let block = Block::default().borders(Borders::ALL);

        // calculate the inner area inside the box
        let mut inner = block.inner(preview_area);
        // 1 column gap on either side
        inner.x += 1;
        inner.width = inner.width.saturating_sub(2);

        block.render(preview_area, surface);

        if let Some((doc, line)) = self.current_file(cx.editor).and_then(|(path, line)| {
            cx.editor
                .document_by_path(&path)
                .or_else(|| self.preview_cache.get(&path))
                .zip(Some(line))
        }) {
            // align to middle
            let first_line = line.unwrap_or(0).saturating_sub(inner.height as usize / 2);
            let offset = Position::new(first_line, 0);

            let highlights = EditorView::doc_syntax_highlights(
                doc,
                offset,
                area.height,
                &cx.editor.theme,
                &cx.editor.syn_loader,
            );
            EditorView::render_text_highlights(
                doc,
                offset,
                inner,
                surface,
                &cx.editor.theme,
                highlights,
            );

            // highlight the line
            if let Some(line) = line {
                for x in inner.left()..inner.right() {
                    surface
                        .get_mut(x, inner.y + line.saturating_sub(first_line) as u16)
                        .set_style(cx.editor.theme.get("ui.selection.primary"));
                }
            }
        }
    }

    fn handle_event(&mut self, event: Event, ctx: &mut Context) -> EventResult {
        // TODO: keybinds for scrolling preview
        self.picker.handle_event(event, ctx)
    }

    fn cursor(&self, area: Rect, ctx: &Editor) -> (Option<Position>, CursorKind) {
        self.picker.cursor(area, ctx)
    }
}

pub struct Picker<T> {
    options: Vec<T>,
    // filter: String,
    matcher: Box<Matcher>,
    /// (index, score)
    matches: Vec<(usize, i64)>,
    /// Filter over original options.
    filters: Vec<usize>, // could be optimized into bit but not worth it now

    cursor: usize,
    // pattern: String,
    prompt: Prompt,
    /// Whether to render in the middle of the area
    render_centered: bool,

    format_fn: Box<dyn Fn(&T) -> Cow<str>>,
    callback_fn: Box<dyn Fn(&mut Editor, &T, Action)>,
}

impl<T> Picker<T> {
    pub fn new(
        render_centered: bool,
        options: Vec<T>,
        format_fn: impl Fn(&T) -> Cow<str> + 'static,
        callback_fn: impl Fn(&mut Editor, &T, Action) + 'static,
    ) -> Self {
        let prompt = Prompt::new(
            "".to_string(),
            None,
            |_pattern: &str| Vec::new(),
            |_editor: &mut Context, _pattern: &str, _event: PromptEvent| {
                //
            },
        );

        let mut picker = Self {
            options,
            matcher: Box::new(Matcher::default()),
            matches: Vec::new(),
            filters: Vec::new(),
            cursor: 0,
            prompt,
            render_centered,
            format_fn: Box::new(format_fn),
            callback_fn: Box::new(callback_fn),
        };

        // TODO: scoring on empty input should just use a fastpath
        picker.score();

        picker
    }

    pub fn score(&mut self) {
        // need to borrow via pattern match otherwise it complains about simultaneous borrow
        let Self {
            ref mut matcher,
            ref mut matches,
            ref filters,
            ref format_fn,
            ..
        } = *self;

        let pattern = &self.prompt.line;

        // reuse the matches allocation
        matches.clear();
        matches.extend(
            self.options
                .iter()
                .enumerate()
                .filter_map(|(index, option)| {
                    // filter options first before matching
                    if !filters.is_empty() {
                        filters.binary_search(&index).ok()?;
                    }
                    // TODO: maybe using format_fn isn't the best idea here
                    let text = (format_fn)(option);
                    // TODO: using fuzzy_indices could give us the char idx for match highlighting
                    matcher
                        .fuzzy_match(&text, pattern)
                        .map(|score| (index, score))
                }),
        );
        matches.sort_unstable_by_key(|(_, score)| -score);

        // reset cursor position
        self.cursor = 0;
    }

    pub fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        if self.matches.is_empty() {
            return;
        }

        if self.cursor < self.matches.len() - 1 {
            self.cursor += 1;
        }
    }

    pub fn selection(&self) -> Option<&T> {
        self.matches
            .get(self.cursor)
            .map(|(index, _score)| &self.options[*index])
    }

    pub fn save_filter(&mut self) {
        self.filters.clear();
        self.filters
            .extend(self.matches.iter().map(|(index, _)| *index));
        self.filters.sort_unstable(); // used for binary search later
        self.prompt.clear();
    }
}

// process:
// - read all the files into a list, maxed out at a large value
// - on input change:
//  - score all the names in relation to input

fn inner_rect(area: Rect) -> Rect {
    let padding_vertical = area.height * 10 / 100;
    let padding_horizontal = area.width * 10 / 100;

    Rect::new(
        area.x + padding_horizontal,
        area.y + padding_vertical,
        area.width - padding_horizontal * 2,
        area.height - padding_vertical * 2,
    )
}

impl<T: 'static> Component for Picker<T> {
    fn handle_event(&mut self, event: Event, cx: &mut Context) -> EventResult {
        let key_event = match event {
            Event::Key(event) => event,
            Event::Resize(..) => return EventResult::Consumed(None),
            _ => return EventResult::Ignored,
        };

        let close_fn = EventResult::Consumed(Some(Box::new(|compositor: &mut Compositor| {
            // remove the layer
            compositor.last_picker = compositor.pop();
        })));

        match key_event {
            KeyEvent {
                code: KeyCode::Up, ..
            }
            | KeyEvent {
                code: KeyCode::BackTab,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('p'),
                modifiers: KeyModifiers::CONTROL,
            } => {
                self.move_up();
            }
            KeyEvent {
                code: KeyCode::Down,
                ..
            }
            | KeyEvent {
                code: KeyCode::Tab, ..
            }
            | KeyEvent {
                code: KeyCode::Char('n'),
                modifiers: KeyModifiers::CONTROL,
            } => {
                self.move_down();
            }
            KeyEvent {
                code: KeyCode::Esc, ..
            }
            | KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
            } => {
                return close_fn;
            }
            KeyEvent {
                code: KeyCode::Enter,
                ..
            } => {
                if let Some(option) = self.selection() {
                    (self.callback_fn)(&mut cx.editor, option, Action::Replace);
                }
                return close_fn;
            }
            KeyEvent {
                code: KeyCode::Char('h'),
                modifiers: KeyModifiers::CONTROL,
            } => {
                if let Some(option) = self.selection() {
                    (self.callback_fn)(&mut cx.editor, option, Action::HorizontalSplit);
                }
                return close_fn;
            }
            KeyEvent {
                code: KeyCode::Char('v'),
                modifiers: KeyModifiers::CONTROL,
            } => {
                if let Some(option) = self.selection() {
                    (self.callback_fn)(&mut cx.editor, option, Action::VerticalSplit);
                }
                return close_fn;
            }
            KeyEvent {
                code: KeyCode::Char(' '),
                modifiers: KeyModifiers::CONTROL,
            } => {
                self.save_filter();
            }
            _ => {
                if let EventResult::Consumed(_) = self.prompt.handle_event(event, cx) {
                    // TODO: recalculate only if pattern changed
                    self.score();
                }
            }
        }

        EventResult::Consumed(None)
    }

    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        let area = if self.render_centered {
            inner_rect(area)
        } else {
            area
        };

        // -- Render the frame:
        // clear area
        let background = cx.editor.theme.get("ui.background");
        surface.clear_with(area, background);

        // don't like this but the lifetime sucks
        let block = Block::default().borders(Borders::ALL);

        // calculate the inner area inside the box
        let inner = block.inner(area);

        block.render(area, surface);

        // -- Render the input bar:

        let area = Rect::new(inner.x + 1, inner.y, inner.width - 1, 1);
        self.prompt.render(area, surface, cx);

        // -- Separator
        let sep_style = Style::default().fg(Color::Rgb(90, 89, 119));
        let borders = BorderType::line_symbols(BorderType::Plain);
        for x in inner.left()..inner.right() {
            surface
                .get_mut(x, inner.y + 1)
                .set_symbol(borders.horizontal)
                .set_style(sep_style);
        }

        // -- Render the contents:
        // subtract the area of the prompt (-2) and current item marker " > " (-3)
        let inner = Rect::new(inner.x + 3, inner.y + 2, inner.width - 3, inner.height - 2);

        let style = cx.editor.theme.get("ui.text");
        let selected = Style::default().fg(Color::Rgb(255, 255, 255));

        let rows = inner.height;
        let offset = self.cursor / (rows as usize) * (rows as usize);

        let files = self.matches.iter().skip(offset).map(|(index, _score)| {
            (index, self.options.get(*index).unwrap()) // get_unchecked
        });

        for (i, (_index, option)) in files.take(rows as usize).enumerate() {
            if i == (self.cursor - offset) {
                surface.set_string(inner.x - 2, inner.y + i as u16, ">", selected);
            }

            surface.set_string_truncated(
                inner.x,
                inner.y + i as u16,
                (self.format_fn)(option),
                inner.width as usize,
                if i == (self.cursor - offset) {
                    selected
                } else {
                    style
                },
                true,
            );
        }
    }

    fn cursor(&self, area: Rect, editor: &Editor) -> (Option<Position>, CursorKind) {
        // TODO: this is mostly duplicate code
        let area = inner_rect(area);
        let block = Block::default().borders(Borders::ALL);
        // calculate the inner area inside the box
        let inner = block.inner(area);

        // prompt area
        let area = Rect::new(inner.x + 1, inner.y, inner.width - 1, 1);

        self.prompt.cursor(area, editor)
    }
}