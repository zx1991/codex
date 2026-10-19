use super::*;
use crate::render::renderable::Renderable;
use codex_config::types::TuiReaderAlignment;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use regex::RegexBuilder;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::ops::Range;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[cfg(test)]
const DEFAULT_READER_HEIGHT: u16 = 3;
#[cfg(test)]
const DEFAULT_PAGE_WIDTH: u16 = 80;
const DEFAULT_READER_WIDTH: u16 = 88;
const LEGACY_PAGE_GRAPHEME_LIMIT: usize = 35;
const MAX_SEARCH_MATCHES: usize = 10_000;
const MAX_RECENT_READER_FILES: usize = 20;
const PROGRESS_FILE_NAME: &str = "reader-progress.json";
const PROGRESS_SAVE_INTERVAL: Duration = Duration::from_millis(750);

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Reader {
    path: PathBuf,
    text: String,
    pages: Vec<ReaderPage>,
    current_page: usize,
    visible: bool,
    page_input: Option<String>,
    search_input: Option<String>,
    search_query: Option<String>,
    search_matches: Vec<Range<usize>>,
    search_matches_truncated: bool,
    current_search_match: Option<usize>,
    configured_height: u16,
    configured_width: u16,
    alignment: TuiReaderAlignment,
    terminal_width: u16,
    layout_width: u16,
    layout_height: u16,
    pagination_status_width: usize,
    progress_path: PathBuf,
    progress_key: String,
    file_fingerprint: String,
    progress_dirty: bool,
    last_progress_save: Instant,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ReaderPage {
    start_byte_offset: usize,
    start_grapheme_offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReaderKeyAction {
    Unhandled,
    Handled,
    Close,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct ProgressStore {
    #[serde(default)]
    entries: HashMap<String, ReaderProgress>,
    #[serde(default)]
    last_path: Option<PathBuf>,
    #[serde(default)]
    recent_paths: Vec<PathBuf>,
}

#[derive(Debug, Deserialize, Serialize)]
struct ReaderProgress {
    file_fingerprint: String,
    page: usize,
    page_grapheme_limit: usize,
    #[serde(default)]
    grapheme_offset: Option<usize>,
}

impl Reader {
    #[cfg(test)]
    pub(crate) fn open(path: &Path, codex_home: &Path, cwd: &Path) -> io::Result<Self> {
        Self::open_with_layout(
            path,
            codex_home,
            cwd,
            DEFAULT_PAGE_WIDTH,
            DEFAULT_READER_HEIGHT,
        )
    }

    pub(super) fn open_with_layout(
        path: &Path,
        codex_home: &Path,
        cwd: &Path,
        width: u16,
        height: u16,
    ) -> io::Result<Self> {
        let path = resolve_path(path, cwd);
        let bytes = fs::read(&path)?;
        let file_fingerprint = digest(&bytes);
        let decoded_text = decode_text(&bytes);
        drop(bytes);
        let text = normalize_text(decoded_text);
        let canonical_path = fs::canonicalize(&path).unwrap_or(path);
        let progress_key = digest(canonical_path.to_string_lossy().as_bytes());
        let progress_path = codex_home.join(PROGRESS_FILE_NAME);
        let saved_offset = load_grapheme_offset(&progress_path, &progress_key, &file_fingerprint);

        let mut reader = Self {
            path: canonical_path,
            text,
            pages: Vec::new(),
            current_page: 0,
            visible: true,
            page_input: None,
            search_input: None,
            search_query: None,
            search_matches: Vec::new(),
            search_matches_truncated: false,
            current_search_match: None,
            configured_height: height.clamp(1, 12),
            configured_width: DEFAULT_READER_WIDTH,
            alignment: TuiReaderAlignment::default(),
            terminal_width: 0,
            layout_width: 0,
            layout_height: 0,
            pagination_status_width: 0,
            progress_path,
            progress_key,
            file_fingerprint,
            progress_dirty: true,
            last_progress_save: Instant::now(),
        };
        reader.reflow(width, reader.configured_height);
        reader.current_page = reader.page_for_offset(saved_offset);
        Ok(reader)
    }

    pub(crate) fn save_progress(&mut self) {
        let mut store = read_progress_store(&self.progress_path);
        store.entries.insert(
            self.progress_key.clone(),
            ReaderProgress {
                file_fingerprint: self.file_fingerprint.clone(),
                page: self.current_page + 1,
                page_grapheme_limit: LEGACY_PAGE_GRAPHEME_LIMIT,
                grapheme_offset: Some(self.current_grapheme_offset()),
            },
        );
        store.last_path = Some(self.path.clone());
        store.recent_paths.retain(|path| path != &self.path);
        store.recent_paths.insert(0, self.path.clone());
        store.recent_paths.truncate(MAX_RECENT_READER_FILES);

        let Some(parent) = self.progress_path.parent() else {
            return;
        };
        if let Err(error) = fs::create_dir_all(parent) {
            tracing::warn!(error = %error, "failed to create reader progress directory");
            return;
        }

        let Ok(serialized) = serde_json::to_vec_pretty(&store) else {
            return;
        };
        let temporary_path = parent.join(format!(".reader-progress-{}.tmp", std::process::id()));
        if let Err(error) = fs::write(&temporary_path, &serialized) {
            tracing::warn!(error = %error, "failed to write reader progress");
            return;
        }
        if let Err(error) = fs::rename(&temporary_path, &self.progress_path) {
            tracing::warn!(error = %error, "failed to commit reader progress");
            let _ = fs::remove_file(temporary_path);
            return;
        }
        self.progress_dirty = false;
        self.last_progress_save = Instant::now();
    }

    fn save_progress_if_due(&mut self) {
        self.save_progress_if_due_at(Instant::now());
    }

    fn save_progress_if_due_at(&mut self, now: Instant) {
        self.progress_dirty = true;
        if now.saturating_duration_since(self.last_progress_save) >= PROGRESS_SAVE_INTERVAL {
            self.save_progress();
        }
    }

    pub(crate) fn visible(&self) -> bool {
        self.visible
    }

    pub(crate) fn toggle_visibility(&mut self) {
        self.visible = !self.visible;
        self.page_input = None;
        self.search_input = None;
        self.save_progress();
    }

    pub(crate) fn page_count(&self) -> usize {
        self.pages.len()
    }

    pub(crate) fn current_page(&self) -> usize {
        self.current_page + 1
    }

    pub(crate) fn set_configured_height(&mut self, height: u16) {
        let height = height.clamp(1, 12);
        if self.configured_height != height {
            self.configured_height = height;
            self.reflow(self.terminal_width.max(1), height);
        }
    }

    pub(crate) fn set_configured_width(&mut self, width: Option<u16>) {
        let width = width.unwrap_or(DEFAULT_READER_WIDTH).clamp(20, 240);
        if self.configured_width != width {
            self.configured_width = width;
            self.reflow(self.terminal_width.max(1), self.layout_height.max(1));
        }
    }

    pub(crate) fn set_alignment(&mut self, alignment: TuiReaderAlignment) {
        self.alignment = alignment;
    }

    pub(crate) fn configured_height(&self) -> u16 {
        u16::from(self.visible) * self.configured_height
    }

    pub(crate) fn reflow(&mut self, width: u16, height: u16) {
        let terminal_width = width.max(1);
        let width = terminal_width.min(self.configured_width);
        let height = height.max(1);
        let status_width = self.search_status_width();
        if self.layout_width == width
            && self.layout_height == height
            && self.pagination_status_width == status_width
        {
            self.terminal_width = terminal_width;
            return;
        }

        let current_offset = self.current_grapheme_offset();
        self.pages = paginate_with_extra_status(&self.text, width, height, status_width);
        self.terminal_width = terminal_width;
        self.layout_width = width;
        self.layout_height = height;
        self.pagination_status_width = status_width;
        self.current_page = self.page_for_offset(current_offset);
    }

    fn current_grapheme_offset(&self) -> usize {
        self.pages
            .get(self.current_page)
            .map_or(0, |page| page.start_grapheme_offset)
    }

    fn page_for_offset(&self, offset: usize) -> usize {
        self.pages
            .partition_point(|page| page.start_grapheme_offset <= offset)
            .saturating_sub(1)
            .min(self.pages.len().saturating_sub(1))
    }

    fn page_for_byte_offset(&self, offset: usize) -> usize {
        self.pages
            .partition_point(|page| page.start_byte_offset <= offset)
            .saturating_sub(1)
            .min(self.pages.len().saturating_sub(1))
    }

    fn handle_key_event(&mut self, key_event: KeyEvent) -> ReaderKeyAction {
        if !matches!(key_event.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return ReaderKeyAction::Unhandled;
        }

        if is_control_key(key_event, 'f') {
            self.start_search();
            return ReaderKeyAction::Handled;
        }

        if self.search_input.is_some() {
            match key_event.code {
                KeyCode::Esc => {
                    self.search_input = None;
                }
                KeyCode::Enter => self.submit_search(),
                KeyCode::Backspace => {
                    if let Some(query) = self.search_input.as_mut()
                        && let Some((byte_index, _)) = query.grapheme_indices(true).next_back()
                    {
                        query.truncate(byte_index);
                    }
                }
                KeyCode::Char(character)
                    if !key_event
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        && !matches!(character, '\n' | '\r')
                        && self
                            .search_input
                            .as_ref()
                            .is_some_and(|query| query.len() + character.len_utf8() <= 256) =>
                {
                    if let Some(query) = self.search_input.as_mut() {
                        query.push(character);
                    }
                }
                _ => {}
            }
            return if is_control_key(key_event, 'c') {
                ReaderKeyAction::Unhandled
            } else {
                ReaderKeyAction::Handled
            };
        }

        let max_page_digits = self.page_count().to_string().len();
        if let Some(page_input) = self.page_input.as_mut() {
            match key_event.code {
                KeyCode::Esc => self.page_input = None,
                KeyCode::Enter => {
                    if let Ok(page) = page_input.parse::<usize>() {
                        self.current_page = page.saturating_sub(1).min(self.pages.len() - 1);
                        self.page_input = None;
                        self.save_progress_if_due();
                    }
                }
                KeyCode::Backspace => {
                    if let Some((byte_index, _)) = page_input.grapheme_indices(true).next_back() {
                        page_input.truncate(byte_index);
                    }
                }
                KeyCode::Char(character)
                    if key_event.modifiers == KeyModifiers::NONE
                        && character.is_ascii_digit()
                        && page_input.len() < max_page_digits =>
                {
                    page_input.push(character);
                }
                _ => {}
            }
            return if is_control_key(key_event, 'c') {
                ReaderKeyAction::Unhandled
            } else {
                ReaderKeyAction::Handled
            };
        }

        if is_control_key(key_event, 'g') {
            self.page_input = Some(String::new());
            return ReaderKeyAction::Handled;
        }

        if is_control_key(key_event, 'c') {
            return ReaderKeyAction::Unhandled;
        }

        match key_event.code {
            KeyCode::Esc => ReaderKeyAction::Close,
            KeyCode::Left | KeyCode::Char('h') if key_event.modifiers == KeyModifiers::NONE => {
                self.current_page = self.current_page.saturating_sub(1);
                self.save_progress_if_due();
                ReaderKeyAction::Handled
            }
            KeyCode::Right | KeyCode::Char('l') if key_event.modifiers == KeyModifiers::NONE => {
                self.current_page = (self.current_page + 1).min(self.pages.len() - 1);
                self.save_progress_if_due();
                ReaderKeyAction::Handled
            }
            KeyCode::Char('n')
                if key_event.modifiers == KeyModifiers::NONE && !self.search_matches.is_empty() =>
            {
                self.move_search_match(/*reverse*/ false);
                ReaderKeyAction::Handled
            }
            KeyCode::Char('N')
                if key_event.modifiers == KeyModifiers::SHIFT
                    && !self.search_matches.is_empty() =>
            {
                self.move_search_match(/*reverse*/ true);
                ReaderKeyAction::Handled
            }
            _ => ReaderKeyAction::Unhandled,
        }
    }

    fn start_search(&mut self) {
        self.page_input = None;
        self.search_query = None;
        self.search_matches.clear();
        self.search_matches_truncated = false;
        self.current_search_match = None;
        self.search_input = Some(String::new());
        self.reflow(self.terminal_width.max(1), self.layout_height.max(1));
    }

    fn submit_search(&mut self) {
        let query = self.search_input.take().unwrap_or_default();
        if query.trim().is_empty() {
            self.search_query = None;
            self.search_matches.clear();
            self.search_matches_truncated = false;
            self.current_search_match = None;
            self.reflow(self.terminal_width.max(1), self.layout_height.max(1));
            return;
        }

        let current_byte_offset = self
            .pages
            .get(self.current_page)
            .map_or(0, |page| page.start_byte_offset);
        let (matches, truncated) = find_matches(&self.text, &query);
        self.search_query = Some(query);
        self.search_matches = matches;
        self.search_matches_truncated = truncated;
        self.current_search_match = None;
        self.reflow(self.terminal_width.max(1), self.layout_height.max(1));

        if !self.search_matches.is_empty() {
            let index = self
                .search_matches
                .partition_point(|matched| matched.start < current_byte_offset)
                % self.search_matches.len();
            self.current_search_match = Some(index);
            self.current_page = self.page_for_byte_offset(self.search_matches[index].start);
            self.save_progress_if_due();
        }
    }

    fn move_search_match(&mut self, reverse: bool) {
        if self.search_matches.is_empty() {
            return;
        }
        let current = self.current_search_match.unwrap_or_default();
        let next = if reverse {
            if current == 0 {
                self.search_matches.len() - 1
            } else {
                current - 1
            }
        } else {
            (current + 1) % self.search_matches.len()
        };
        self.current_search_match = Some(next);
        self.current_page = self.page_for_byte_offset(self.search_matches[next].start);
        self.save_progress_if_due();
    }

    fn search_status_width(&self) -> usize {
        if self.search_query.is_none() {
            return 0;
        }
        let suffix = if self.search_matches.is_empty() {
            " [no matches]".to_string()
        } else {
            let total = if self.search_matches_truncated {
                format!("{}+", self.search_matches.len())
            } else {
                self.search_matches.len().to_string()
            };
            format!(" [find {total}/{total}]")
        };
        UnicodeWidthStr::width(suffix.as_str())
    }

    fn display_lines(&self) -> Vec<Line<'static>> {
        let height = usize::from(self.layout_height.max(1));
        let status_width = page_status_width(self.page_count(), self.search_status_width());
        let page_lines = self
            .pages
            .get(self.current_page)
            .map(|page| {
                page_line_ranges(
                    &self.text,
                    page.start_byte_offset,
                    self.layout_width,
                    self.layout_height,
                    status_width,
                )
            })
            .unwrap_or_default();
        let status = self.status_text();
        let mut lines = Vec::with_capacity(height);

        for row in 0..height {
            if row == height - 1
                && let Some(query) = self.search_input.as_ref()
            {
                let prompt = format!("[find: {query}_]");
                lines.push(Line::from(
                    truncate_to_width(&prompt, usize::from(self.layout_width)).dim(),
                ));
                continue;
            }

            let range = page_lines.get(row);
            lines.push(self.render_line(range, (row == height - 1).then_some(status.as_str())));
        }

        lines
    }

    fn status_text(&self) -> String {
        let page_count = self.page_count();
        let page_status = match self.page_input.as_deref() {
            Some("") => format!("[?/{page_count}]"),
            Some(page_input) => format!("[{page_input}/{page_count}]"),
            None => format!("[{}/{}]", self.current_page(), page_count),
        };
        if self.search_query.is_none() {
            return page_status;
        }
        if self.search_matches.is_empty() {
            return format!("{page_status} [no matches]");
        }

        let current = self.current_search_match.unwrap_or_default() + 1;
        let total = if self.search_matches_truncated {
            format!("{}+", self.search_matches.len())
        } else {
            self.search_matches.len().to_string()
        };
        format!("{page_status} [find {current}/{total}]")
    }

    fn render_line(&self, range: Option<&Range<usize>>, status: Option<&str>) -> Line<'static> {
        let text_range = range.cloned().unwrap_or(0..0);
        let source = &self.text[text_range.clone()];
        let mut spans = Vec::with_capacity(4);
        let active_match = self
            .current_search_match
            .and_then(|index| self.search_matches.get(index))
            .filter(|matched| matched.start >= text_range.start && matched.end <= text_range.end);

        if let Some(matched) = active_match {
            let dim = Style::default().add_modifier(Modifier::DIM);
            if matched.start > text_range.start {
                spans.push(Span::styled(
                    self.text[text_range.start..matched.start].to_string(),
                    dim,
                ));
            }
            spans.push(Span::styled(
                self.text[matched.clone()].to_string(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
            if matched.end < text_range.end {
                spans.push(Span::styled(
                    self.text[matched.end..text_range.end].to_string(),
                    dim,
                ));
            }
        } else if !source.is_empty() {
            spans.push(Span::styled(
                source.to_string(),
                Style::default().add_modifier(Modifier::DIM),
            ));
        }

        if let Some(status) = status {
            let separator = if source.is_empty() { "" } else { " " };
            spans.push(Span::styled(
                format!("{separator}{status}"),
                Style::default().add_modifier(Modifier::DIM),
            ));
        }

        Line::from(spans)
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        if self.progress_dirty {
            self.save_progress();
        }
    }
}

impl Renderable for Reader {
    fn render(&self, area: Rect, buffer: &mut Buffer) {
        if !self.visible || area.is_empty() {
            return;
        }
        Clear.render(area, buffer);
        let content_width = self.layout_width.min(area.width);
        let horizontal_offset = match self.alignment {
            TuiReaderAlignment::Left => 0,
            TuiReaderAlignment::Center => area.width.saturating_sub(content_width) / 2,
            TuiReaderAlignment::Right => area.width.saturating_sub(content_width),
        };
        let content_area = Rect::new(
            area.x.saturating_add(horizontal_offset),
            area.y,
            content_width,
            area.height,
        );
        Paragraph::new(self.display_lines())
            .alignment(ratatui::layout::Alignment::Left)
            .render(content_area, buffer);
    }

    fn desired_height(&self, _width: u16) -> u16 {
        self.configured_height()
    }
}

impl App {
    pub(super) fn open_reader(&mut self, tui: &mut tui::Tui, path: PathBuf) {
        match Reader::open_with_layout(
            &path,
            self.config.codex_home.as_path(),
            self.config.cwd.as_path(),
            tui.terminal.last_known_screen_size.width,
            self.local_settings.tui.reader_height.unwrap_or(3),
        ) {
            Ok(mut reader) => {
                reader.set_configured_height(self.local_settings.tui.reader_height.unwrap_or(3));
                let configured_height = reader.configured_height();
                reader.set_configured_width(self.local_settings.tui.reader_width);
                reader.set_alignment(self.local_settings.tui.reader_alignment);
                reader.reflow(tui.terminal.last_known_screen_size.width, configured_height);
                let previous_reader = self.reader.replace(reader);
                if let Some(mut previous_reader) = previous_reader {
                    previous_reader.save_progress();
                }
                if let Some(reader) = self.reader.as_mut() {
                    reader.save_progress();
                }
                tui.frame_requester().schedule_frame();
            }
            Err(error) => {
                self.chat_widget
                    .add_error_message(format!("Cannot open reader '{}': {error}", path.display()));
            }
        }
    }

    pub(super) fn open_previous_reader(&mut self, tui: &mut tui::Tui) {
        let path = self
            .reader
            .as_ref()
            .map(|reader| reader.path.clone())
            .or_else(|| {
                read_progress_store(&self.config.codex_home.as_path().join(PROGRESS_FILE_NAME))
                    .last_path
            });

        let Some(path) = path else {
            self.chat_widget
                .add_error_message("No previous reader file to open.".to_string());
            return;
        };

        self.open_reader(tui, path);
    }

    pub(super) fn open_recent_reader(&mut self, tui: &mut tui::Tui, index: Option<usize>) {
        let paths = recent_reader_paths(&self.config.codex_home.as_path().join(PROGRESS_FILE_NAME));
        if paths.is_empty() {
            self.chat_widget
                .add_info_message("No recent reader files.".to_string(), /*hint*/ None);
            return;
        }

        let Some(index) = index else {
            let listing = paths
                .iter()
                .enumerate()
                .map(|(index, path)| format!("{}. {}", index + 1, path.display()))
                .collect::<Vec<_>>()
                .join("\n");
            self.chat_widget.add_info_message(
                format!("Recent reader files (use `/reader recent N` to open):\n{listing}"),
                /*hint*/ None,
            );
            return;
        };

        if index == 0 || index > paths.len() {
            self.chat_widget.add_error_message(format!(
                "No recent reader file at position {index}. Use `/reader recent` to list files."
            ));
            return;
        }

        self.open_reader(tui, paths[index - 1].clone());
    }

    pub(super) fn handle_reader_key_event(
        &mut self,
        tui: &mut tui::Tui,
        key_event: KeyEvent,
    ) -> bool {
        if key_event.kind == KeyEventKind::Press
            && key_event.code == KeyCode::Char('k')
            && key_event.modifiers == KeyModifiers::NONE
            && let Some(reader) = self.reader.as_mut()
        {
            reader.toggle_visibility();
            tui.frame_requester().schedule_frame();
            return true;
        }

        let Some(reader) = self.reader.as_mut() else {
            return false;
        };
        if !reader.visible() {
            return false;
        }

        match reader.handle_key_event(key_event) {
            ReaderKeyAction::Unhandled => false,
            ReaderKeyAction::Handled => {
                tui.frame_requester().schedule_frame();
                true
            }
            ReaderKeyAction::Close => {
                let reader = self.reader.take();
                if let Some(mut reader) = reader {
                    reader.save_progress();
                }
                tui.frame_requester().schedule_frame();
                true
            }
        }
    }
}

fn is_control_key(key_event: KeyEvent, character: char) -> bool {
    matches!(
        key_event.code,
        KeyCode::Char(key) if key.eq_ignore_ascii_case(&character)
    ) && key_event.modifiers.contains(KeyModifiers::CONTROL)
}

fn normalize_text(text: String) -> String {
    if !text.bytes().any(|byte| matches!(byte, b'\r' | b'\t')) {
        return text;
    }

    let mut normalized = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\r' => {
                if characters.peek() == Some(&'\n') {
                    characters.next();
                }
                normalized.push('\n');
            }
            '\t' => normalized.push_str("    "),
            _ => normalized.push(character),
        }
    }
    normalized
}

#[cfg(test)]
fn paginate(text: &str, width: u16, height: u16) -> Vec<ReaderPage> {
    paginate_with_extra_status(text, width, height, 0)
}

fn paginate_with_extra_status(
    text: &str,
    width: u16,
    height: u16,
    extra_status_width: usize,
) -> Vec<ReaderPage> {
    let width = usize::from(width.max(1));
    let height = usize::from(height.max(1));
    let mut page_status_width = UnicodeWidthStr::width(" [1/1]");

    for _ in 0..8 {
        let status_width = page_status_width + extra_status_width;
        let pages = paginate_with_status_width(text, width, height, status_width);
        let total = pages.len();
        let required_width = UnicodeWidthStr::width(format!(" [{total}/{total}]").as_str());
        if required_width == page_status_width {
            return pages;
        }
        page_status_width = required_width;
    }

    paginate_with_status_width(text, width, height, page_status_width + extra_status_width)
}

fn paginate_with_status_width(
    text: &str,
    width: usize,
    height: usize,
    status_width: usize,
) -> Vec<ReaderPage> {
    let final_row_width = width.saturating_sub(status_width).max(1);
    let mut pages = Vec::new();
    let mut completed_lines = 0usize;
    let mut current_line_width = 0usize;
    let mut current_line_start_byte = 0usize;
    let mut current_line_start_grapheme = 0usize;
    let mut grapheme_offset = 0usize;

    for (byte_index, grapheme) in text.grapheme_indices(true) {
        if grapheme == "\n" {
            push_reader_line(
                &mut pages,
                &mut completed_lines,
                height,
                current_line_start_byte,
                current_line_start_grapheme,
            );
            current_line_width = 0;
            grapheme_offset += 1;
            current_line_start_byte = byte_index + grapheme.len();
            current_line_start_grapheme = grapheme_offset;
            continue;
        }

        let grapheme_width = UnicodeWidthStr::width(grapheme).max(1);
        loop {
            let row_in_page = completed_lines % height;
            let row_width = if row_in_page == height - 1 {
                final_row_width
            } else {
                width
            };
            if current_line_width + grapheme_width > row_width && current_line_width > 0 {
                push_reader_line(
                    &mut pages,
                    &mut completed_lines,
                    height,
                    current_line_start_byte,
                    current_line_start_grapheme,
                );
                current_line_width = 0;
                current_line_start_byte = byte_index;
                current_line_start_grapheme = grapheme_offset;
                continue;
            }
            current_line_width += grapheme_width;
            break;
        }
        grapheme_offset += 1;
    }

    if current_line_width > 0 || text.is_empty() || text.ends_with('\n') {
        push_reader_line(
            &mut pages,
            &mut completed_lines,
            height,
            current_line_start_byte,
            current_line_start_grapheme,
        );
    }
    if pages.is_empty() {
        pages.push(ReaderPage {
            start_byte_offset: 0,
            start_grapheme_offset: 0,
        });
    }
    pages
}

fn push_reader_line(
    pages: &mut Vec<ReaderPage>,
    completed_lines: &mut usize,
    page_height: usize,
    start_byte_offset: usize,
    start_grapheme_offset: usize,
) {
    if *completed_lines % page_height == 0 {
        pages.push(ReaderPage {
            start_byte_offset,
            start_grapheme_offset,
        });
    }
    *completed_lines += 1;
}

fn page_status_width(page_count: usize, extra_status_width: usize) -> usize {
    UnicodeWidthStr::width(format!(" [{page_count}/{page_count}]").as_str()) + extra_status_width
}

fn page_line_ranges(
    text: &str,
    start_byte_offset: usize,
    width: u16,
    height: u16,
    status_width: usize,
) -> Vec<Range<usize>> {
    let width = usize::from(width.max(1));
    let height = usize::from(height.max(1));
    let final_row_width = width.saturating_sub(status_width).max(1);
    let mut lines = Vec::with_capacity(height);
    let mut line_start = start_byte_offset;
    let mut line_width = 0usize;

    for (relative_byte_index, grapheme) in text[start_byte_offset..].grapheme_indices(true) {
        let byte_index = start_byte_offset + relative_byte_index;
        if grapheme == "\n" {
            lines.push(line_start..byte_index);
            if lines.len() == height {
                return lines;
            }
            line_start = byte_index + grapheme.len();
            line_width = 0;
            continue;
        }

        let grapheme_width = UnicodeWidthStr::width(grapheme).max(1);
        loop {
            let row_width = if lines.len() == height - 1 {
                final_row_width
            } else {
                width
            };
            if line_width + grapheme_width > row_width && line_width > 0 {
                lines.push(line_start..byte_index);
                if lines.len() == height {
                    return lines;
                }
                line_start = byte_index;
                line_width = 0;
                continue;
            }
            line_width += grapheme_width;
            break;
        }
    }

    if lines.len() < height && (line_width > 0 || text.is_empty() || text.ends_with('\n')) {
        lines.push(line_start..text.len());
    }
    lines
}

fn find_matches(text: &str, query: &str) -> (Vec<Range<usize>>, bool) {
    if query.is_empty() || query.contains('\n') || query.contains('\r') {
        return (Vec::new(), false);
    }
    let Ok(pattern) = RegexBuilder::new(&regex::escape(query))
        .case_insensitive(true)
        .build()
    else {
        return (Vec::new(), false);
    };

    let mut matches = Vec::with_capacity(MAX_SEARCH_MATCHES.min(128));
    let mut truncated = false;
    for matched in pattern.find_iter(text) {
        if matches.len() == MAX_SEARCH_MATCHES {
            truncated = true;
            break;
        }
        matches.push(matched.range());
    }
    (matches, truncated)
}

fn decode_text(bytes: &[u8]) -> String {
    if let Some(bytes) = bytes.strip_prefix(&[0xff, 0xfe, 0x00, 0x00]) {
        decode_utf32(bytes, /*little_endian*/ true)
    } else if let Some(bytes) = bytes.strip_prefix(&[0x00, 0x00, 0xfe, 0xff]) {
        decode_utf32(bytes, /*little_endian*/ false)
    } else if let Some(bytes) = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]) {
        String::from_utf8_lossy(bytes).into_owned()
    } else if let Some(bytes) = bytes.strip_prefix(&[0xff, 0xfe]) {
        decode_utf16(bytes, /*little_endian*/ true)
    } else if let Some(bytes) = bytes.strip_prefix(&[0xfe, 0xff]) {
        decode_utf16(bytes, /*little_endian*/ false)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

fn decode_utf16(bytes: &[u8], little_endian: bool) -> String {
    let units = bytes.chunks_exact(2).map(|pair| {
        let pair = [pair[0], pair[1]];
        if little_endian {
            u16::from_le_bytes(pair)
        } else {
            u16::from_be_bytes(pair)
        }
    });
    let mut text = char::decode_utf16(units)
        .map(|result| result.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect::<String>();
    if bytes.len() % 2 != 0 {
        text.push(char::REPLACEMENT_CHARACTER);
    }
    text
}

fn decode_utf32(bytes: &[u8], little_endian: bool) -> String {
    let mut text = bytes
        .chunks_exact(4)
        .map(|chunk| {
            let chunk = [chunk[0], chunk[1], chunk[2], chunk[3]];
            let codepoint = if little_endian {
                u32::from_le_bytes(chunk)
            } else {
                u32::from_be_bytes(chunk)
            };
            char::from_u32(codepoint).unwrap_or(char::REPLACEMENT_CHARACTER)
        })
        .collect::<String>();
    if bytes.len() % 4 != 0 {
        text.push(char::REPLACEMENT_CHARACTER);
    }
    text
}

fn truncate_to_width(text: &str, width: usize) -> String {
    let mut output = String::new();
    let mut used_width = 0usize;
    for grapheme in text.graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if used_width + grapheme_width > width {
            break;
        }
        output.push_str(grapheme);
        used_width += grapheme_width;
    }
    output
}

fn recent_reader_paths(path: &Path) -> Vec<PathBuf> {
    let store = read_progress_store(path);
    let paths = if store.recent_paths.is_empty() {
        store.last_path.into_iter().collect()
    } else {
        store.recent_paths
    };
    paths
        .into_iter()
        .filter(|path| path.is_file())
        .take(MAX_RECENT_READER_FILES)
        .collect()
}

fn resolve_path(path: &Path, cwd: &Path) -> PathBuf {
    let path = path.to_string_lossy();
    let path = if path == "~" {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"))
    } else if let Some(path) = path.strip_prefix("~/") {
        dirs::home_dir()
            .map(|home| home.join(path))
            .unwrap_or_else(|| PathBuf::from(format!("~/{path}")))
    } else {
        PathBuf::from(path.as_ref())
    };
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn read_progress_store(path: &Path) -> ProgressStore {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn load_grapheme_offset(path: &Path, key: &str, fingerprint: &str) -> usize {
    read_progress_store(path)
        .entries
        .get(key)
        .filter(|progress| progress.file_fingerprint == fingerprint)
        .map(|progress| {
            progress.grapheme_offset.unwrap_or_else(|| {
                progress
                    .page
                    .saturating_sub(1)
                    .saturating_mul(progress.page_grapheme_limit)
            })
        })
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "reader_tests.rs"]
mod tests;
