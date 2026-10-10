use super::*;
use crate::render::renderable::Renderable;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use ratatui::buffer::Buffer;
use ratatui::layout::Alignment;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[cfg(test)]
const DEFAULT_READER_HEIGHT: u16 = 3;
#[cfg(test)]
const DEFAULT_PAGE_WIDTH: u16 = 80;
const LEGACY_PAGE_GRAPHEME_LIMIT: usize = 35;
const PROGRESS_FILE_NAME: &str = "reader-progress.json";

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Reader {
    path: PathBuf,
    text: String,
    pages: Vec<ReaderPage>,
    current_page: usize,
    visible: bool,
    page_input: Option<String>,
    configured_height: u16,
    layout_width: u16,
    layout_height: u16,
    progress_path: PathBuf,
    progress_key: String,
    file_fingerprint: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ReaderPage {
    lines: Vec<String>,
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
        let text = normalize_text(&String::from_utf8_lossy(&bytes));
        let canonical_path = fs::canonicalize(&path).unwrap_or(path);
        let progress_key = digest(canonical_path.to_string_lossy().as_bytes());
        let file_fingerprint = digest(&bytes);
        let progress_path = codex_home.join(PROGRESS_FILE_NAME);
        let saved_offset = load_grapheme_offset(&progress_path, &progress_key, &file_fingerprint);

        let mut reader = Self {
            path: canonical_path,
            text,
            pages: Vec::new(),
            current_page: 0,
            visible: true,
            page_input: None,
            configured_height: height.clamp(1, 12),
            layout_width: 0,
            layout_height: 0,
            progress_path,
            progress_key,
            file_fingerprint,
        };
        reader.reflow(width, reader.configured_height);
        reader.current_page = reader.page_for_offset(saved_offset);
        Ok(reader)
    }

    pub(crate) fn save_progress(&self) {
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
        }
    }

    pub(crate) fn visible(&self) -> bool {
        self.visible
    }

    pub(crate) fn toggle_visibility(&mut self) {
        self.visible = !self.visible;
        self.page_input = None;
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
            self.reflow(self.layout_width.max(1), height);
        }
    }

    pub(crate) fn configured_height(&self) -> u16 {
        u16::from(self.visible) * self.configured_height
    }

    pub(crate) fn reflow(&mut self, width: u16, height: u16) {
        let width = width.max(1);
        let height = height.max(1);
        if self.layout_width == width && self.layout_height == height {
            return;
        }

        let current_offset = self.current_grapheme_offset();
        self.pages = paginate(&self.text, width, height);
        self.layout_width = width;
        self.layout_height = height;
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

    fn handle_key_event(&mut self, key_event: KeyEvent) -> ReaderKeyAction {
        if !matches!(key_event.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return ReaderKeyAction::Unhandled;
        }

        if is_control_key(key_event, 'g') {
            self.page_input = Some(String::new());
            return ReaderKeyAction::Handled;
        }

        let max_page_digits = self.page_count().to_string().len();
        if let Some(page_input) = self.page_input.as_mut() {
            match key_event.code {
                KeyCode::Esc => {
                    self.page_input = None;
                }
                KeyCode::Enter => {
                    if let Ok(page) = page_input.parse::<usize>() {
                        self.current_page = page.saturating_sub(1).min(self.pages.len() - 1);
                        self.page_input = None;
                        self.save_progress();
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

        if is_control_key(key_event, 'c') {
            return ReaderKeyAction::Unhandled;
        }

        match key_event.code {
            KeyCode::Esc => ReaderKeyAction::Close,
            KeyCode::Left | KeyCode::Char('h') if key_event.modifiers == KeyModifiers::NONE => {
                self.current_page = self.current_page.saturating_sub(1);
                self.save_progress();
                ReaderKeyAction::Handled
            }
            KeyCode::Right | KeyCode::Char('l') if key_event.modifiers == KeyModifiers::NONE => {
                self.current_page = (self.current_page + 1).min(self.pages.len() - 1);
                self.save_progress();
                ReaderKeyAction::Handled
            }
            _ => ReaderKeyAction::Unhandled,
        }
    }

    fn display_lines(&self) -> Vec<Line<'static>> {
        let page_count = self.page_count();
        let status = match self.page_input.as_deref() {
            Some("") => format!("[?/{page_count}]"),
            Some(page_input) => format!("[{page_input}/{page_count}]"),
            None => format!("[{}/{}]", self.current_page(), page_count),
        };
        let mut lines = self
            .pages
            .get(self.current_page)
            .map(|page| page.lines.clone())
            .unwrap_or_default();
        lines.resize(usize::from(self.layout_height), String::new());
        let last_line = lines
            .last_mut()
            .expect("reader layout has at least one row");
        if !last_line.is_empty() {
            last_line.push(' ');
        }
        last_line.push_str(&status);
        lines
            .into_iter()
            .map(|line| Line::from(line.dim()))
            .collect()
    }
}

impl Renderable for Reader {
    fn render(&self, area: Rect, buffer: &mut Buffer) {
        if !self.visible || area.is_empty() {
            return;
        }
        Clear.render(area, buffer);
        Paragraph::new(self.display_lines())
            .alignment(Alignment::Left)
            .render(area, buffer);
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
                reader.reflow(tui.terminal.last_known_screen_size.width, configured_height);
                let previous_reader = self.reader.replace(reader);
                if let Some(previous_reader) = previous_reader {
                    previous_reader.save_progress();
                }
                if let Some(reader) = self.reader.as_ref() {
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

    pub(super) fn handle_reader_key_event(
        &mut self,
        tui: &mut tui::Tui,
        key_event: KeyEvent,
    ) -> bool {
        if key_event.kind == KeyEventKind::Press
            && is_control_key(key_event, 'h')
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
                if let Some(reader) = reader {
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

fn normalize_text(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\t', "    ")
}

fn paginate(text: &str, width: u16, height: u16) -> Vec<ReaderPage> {
    let width = usize::from(width.max(1));
    let height = usize::from(height.max(1));
    let mut status_width = UnicodeWidthStr::width(" [1/1]");

    for _ in 0..8 {
        let pages = paginate_with_status_width(text, width, height, status_width);
        let total = pages.len();
        let required_width = UnicodeWidthStr::width(format!(" [{total}/{total}]").as_str());
        if required_width == status_width {
            return pages;
        }
        status_width = required_width;
    }

    paginate_with_status_width(text, width, height, status_width)
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
    let mut current_line = String::new();
    let mut current_line_width = 0usize;
    let mut current_line_start = 0usize;
    let mut grapheme_offset = 0usize;

    for grapheme in text.graphemes(true) {
        if grapheme == "\n" {
            push_reader_line(
                &mut pages,
                &mut completed_lines,
                height,
                std::mem::take(&mut current_line),
                current_line_start,
            );
            current_line_width = 0;
            grapheme_offset += 1;
            current_line_start = grapheme_offset;
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
            if current_line_width + grapheme_width > row_width && !current_line.is_empty() {
                push_reader_line(
                    &mut pages,
                    &mut completed_lines,
                    height,
                    std::mem::take(&mut current_line),
                    current_line_start,
                );
                current_line_width = 0;
                current_line_start = grapheme_offset;
                continue;
            }
            current_line.push_str(grapheme);
            current_line_width += grapheme_width;
            break;
        }
        grapheme_offset += 1;
    }

    if !current_line.is_empty() || text.is_empty() || text.ends_with('\n') {
        push_reader_line(
            &mut pages,
            &mut completed_lines,
            height,
            current_line,
            current_line_start,
        );
    }
    if pages.is_empty() {
        pages.push(ReaderPage {
            lines: vec![String::new()],
            start_grapheme_offset: 0,
        });
    }
    pages
}

fn push_reader_line(
    pages: &mut Vec<ReaderPage>,
    completed_lines: &mut usize,
    page_height: usize,
    line: String,
    start_grapheme_offset: usize,
) {
    if *completed_lines % page_height == 0 {
        pages.push(ReaderPage {
            lines: Vec::with_capacity(page_height),
            start_grapheme_offset,
        });
    }
    if let Some(page) = pages.last_mut() {
        page.lines.push(line);
    }
    *completed_lines += 1;
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
