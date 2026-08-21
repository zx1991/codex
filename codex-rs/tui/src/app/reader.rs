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

const PAGE_GRAPHEME_LIMIT: usize = 35;
const LINE_BREAK_MARKER: &str = "↵";
const PROGRESS_FILE_NAME: &str = "reader-progress.json";

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Reader {
    pages: Vec<String>,
    current_page: usize,
    visible: bool,
    page_input: Option<String>,
    progress_path: PathBuf,
    progress_key: String,
    file_fingerprint: String,
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
}

#[derive(Debug, Deserialize, Serialize)]
struct ReaderProgress {
    file_fingerprint: String,
    page: usize,
    page_grapheme_limit: usize,
}

impl Reader {
    pub(crate) fn open(path: &Path, codex_home: &Path, cwd: &Path) -> io::Result<Self> {
        let path = resolve_path(path, cwd);
        let bytes = fs::read(&path)?;
        let text = String::from_utf8_lossy(&bytes);
        let pages = paginate(&text);
        let canonical_path = fs::canonicalize(&path).unwrap_or(path);
        let progress_key = digest(canonical_path.to_string_lossy().as_bytes());
        let file_fingerprint = digest(&bytes);
        let progress_path = codex_home.join(PROGRESS_FILE_NAME);
        let current_page = load_page(
            &progress_path,
            &progress_key,
            &file_fingerprint,
            pages.len(),
        );

        Ok(Self {
            pages,
            current_page,
            visible: true,
            page_input: None,
            progress_path,
            progress_key,
            file_fingerprint,
        })
    }

    pub(crate) fn save_progress(&self) {
        let mut store = read_progress_store(&self.progress_path);
        store.entries.insert(
            self.progress_key.clone(),
            ReaderProgress {
                file_fingerprint: self.file_fingerprint.clone(),
                page: self.current_page + 1,
                page_grapheme_limit: PAGE_GRAPHEME_LIMIT,
            },
        );

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

    fn handle_key_event(&mut self, key_event: KeyEvent) -> ReaderKeyAction {
        if !matches!(key_event.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return ReaderKeyAction::Unhandled;
        }

        if is_control_key(key_event, 'g') {
            self.page_input = Some(String::new());
            return ReaderKeyAction::Handled;
        }

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
                        && page_input.len() < 9 =>
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

    fn display_line(&self) -> Line<'static> {
        let status = match self.page_input.as_deref() {
            Some("") => format!("[? / {}]", self.page_count()),
            Some(page_input) => format!("[{page_input}/{}]", self.page_count()),
            None => format!("[{}/{}]", self.current_page(), self.page_count()),
        };
        let content = if self.page_input.is_some() {
            ""
        } else {
            {
                self.pages
                    .get(self.current_page)
                    .map(String::as_str)
                    .unwrap_or_default()
            }
        };
        Line::from(vec![status.dim(), " ".dim(), content.to_string().dim()])
    }
}

impl Renderable for Reader {
    fn render(&self, area: Rect, buffer: &mut Buffer) {
        if !self.visible || area.is_empty() {
            return;
        }
        Clear.render(area, buffer);
        Paragraph::new(self.display_line())
            .alignment(Alignment::Left)
            .render(area, buffer);
    }

    fn desired_height(&self, _width: u16) -> u16 {
        u16::from(self.visible)
    }
}

impl App {
    pub(super) fn open_reader(&mut self, tui: &mut tui::Tui, path: PathBuf) {
        match Reader::open(
            &path,
            self.config.codex_home.as_path(),
            self.config.cwd.as_path(),
        ) {
            Ok(reader) => {
                if let Some(previous_reader) = self.reader.replace(reader) {
                    previous_reader.save_progress();
                }
                tui.frame_requester().schedule_frame();
            }
            Err(error) => {
                self.chat_widget
                    .add_error_message(format!("Cannot open reader '{}': {error}", path.display()));
            }
        }
    }

    pub(super) fn handle_reader_key_event(
        &mut self,
        tui: &mut tui::Tui,
        key_event: KeyEvent,
    ) -> bool {
        if self.overlay.is_some() || !self.chat_widget.no_modal_or_popup_active() {
            return false;
        }

        if is_control_key(key_event, 'h')
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

fn paginate(text: &str) -> Vec<String> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let flattened = normalized
        .split('\n')
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join(LINE_BREAK_MARKER);
    if flattened.is_empty() {
        return vec![String::new()];
    }

    flattened
        .graphemes(true)
        .collect::<Vec<_>>()
        .chunks(PAGE_GRAPHEME_LIMIT)
        .map(<[&str]>::concat)
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

fn load_page(path: &Path, key: &str, fingerprint: &str, page_count: usize) -> usize {
    read_progress_store(path)
        .entries
        .get(key)
        .filter(|progress| {
            progress.file_fingerprint == fingerprint
                && progress.page_grapheme_limit == PAGE_GRAPHEME_LIMIT
        })
        .map(|progress| progress.page.saturating_sub(1).min(page_count - 1))
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "reader_tests.rs"]
mod tests;
