use super::*;
use pretty_assertions::assert_eq;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use std::fs;
use tempfile::tempdir;

fn render_rows(reader: &Reader, width: u16, height: u16) -> Vec<String> {
    let area = Rect::new(0, 0, width, height);
    let mut buffer = Buffer::empty(area);
    reader.render(area, &mut buffer);
    buffer
        .content
        .chunks(usize::from(width))
        .take(usize::from(height))
        .map(|row| {
            row.iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

#[test]
fn pages_preserve_source_spacing_and_append_status_to_the_last_row() {
    let temp = tempdir().expect("temp directory");
    let home = temp.path().join("codex");
    let path = temp.path().join("book.txt");
    fs::write(&path, "  第一行\n第二行  \n第三行").expect("write book");

    let mut reader = Reader::open(&path, &home, temp.path()).expect("open book");
    reader.reflow(20, 3);

    assert_eq!(
        reader.pages[0].lines,
        vec!["  第一行", "第二行  ", "第三行"]
    );
    let rows = render_rows(&reader, 20, 3);
    assert_eq!(rows, vec!["  第一行", "第二行  ", "第三行 [1/1]"]);
}

#[test]
fn pagination_wraps_wide_characters_by_terminal_columns() {
    let text = "A界BCDEF";
    let pages = paginate(text, 12, 2);
    let reconstructed = pages
        .iter()
        .flat_map(|page| page.lines.iter())
        .map(String::as_str)
        .collect::<String>();

    assert_eq!(reconstructed, text);
    assert!(pages.iter().all(|page| {
        page.lines.iter().enumerate().all(|(index, line)| {
            UnicodeWidthStr::width(line.as_str()) <= if index == 1 { 6 } else { 12 }
        })
    }));
}

#[test]
fn progress_restores_only_for_the_same_file_content() {
    let temp = tempdir().expect("temp directory");
    let home = temp.path().join("codex");
    let path = temp.path().join("book.txt");
    fs::write(&path, "0123456789".repeat(60)).expect("write book");

    let mut reader = Reader::open_with_layout(&path, &home, temp.path(), 20, 3).expect("open book");
    assert_eq!(reader.current_page(), 1);
    reader.current_page = 2;
    let saved_offset = reader.current_grapheme_offset();
    reader.save_progress();

    let restored = Reader::open_with_layout(&path, &home, temp.path(), 20, 3).expect("reopen book");
    assert_eq!(restored.current_page(), 3);
    assert_eq!(restored.current_grapheme_offset(), saved_offset);

    fs::write(&path, "changed content").expect("change book");
    let reset =
        Reader::open_with_layout(&path, &home, temp.path(), 20, 3).expect("reopen changed book");
    assert_eq!(reset.current_page(), 1);
}

#[test]
fn progress_remembers_the_last_reader_file() {
    let temp = tempdir().expect("temp directory");
    let home = temp.path().join("codex");
    let path = temp.path().join("book.txt");
    fs::write(&path, "reader content").expect("write book");

    let reader = Reader::open(&path, &home, temp.path()).expect("open book");
    reader.save_progress();

    assert_eq!(
        read_progress_store(&home.join(PROGRESS_FILE_NAME)).last_path,
        Some(fs::canonicalize(path).expect("canonicalize book"))
    );
}

#[tokio::test]
async fn ctrl_h_hides_reader_without_consuming_composer_draft() {
    let temp = tempdir().expect("temp directory");
    let path = temp.path().join("book.txt");
    fs::write(&path, "reader content").expect("write book");

    let mut app = crate::app::test_support::make_test_app().await;
    app.reader = Some(Reader::open(&path, temp.path(), temp.path()).expect("open book"));
    app.chat_widget.apply_external_edit("draft".to_string());
    let mut tui = crate::tui::test_support::make_test_tui().expect("create test terminal");
    let ctrl_h = KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL);

    assert!(app.handle_reader_key_event(&mut tui, ctrl_h));
    assert!(
        !app.reader
            .as_ref()
            .expect("reader should be open")
            .visible()
    );
    assert_eq!(app.chat_widget.composer_text_with_pending(), "draft");

    let mut ctrl_h_repeat = ctrl_h;
    ctrl_h_repeat.kind = KeyEventKind::Repeat;
    assert!(!app.handle_reader_key_event(&mut tui, ctrl_h_repeat));
    assert!(
        !app.reader
            .as_ref()
            .expect("reader should remain hidden")
            .visible()
    );

    assert!(app.handle_reader_key_event(&mut tui, ctrl_h));
    assert!(
        app.reader
            .as_ref()
            .expect("reader should be open")
            .visible()
    );
    assert_eq!(app.chat_widget.composer_text_with_pending(), "draft");
}

#[test]
fn page_jump_and_left_aligned_rendering_work_with_multiple_rows() {
    let temp = tempdir().expect("temp directory");
    let path = temp.path().join("book.txt");
    fs::write(&path, "0123456789".repeat(20)).expect("write book");
    let mut reader =
        Reader::open_with_layout(&path, temp.path(), temp.path(), 41, 3).expect("open book");

    let normal = render_rows(&reader, 41, 3);
    assert!(normal[2].ends_with("[1/2]"));

    assert_eq!(
        reader.handle_key_event(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)),
        ReaderKeyAction::Handled
    );
    assert_eq!(reader.current_page(), 2);
    assert_eq!(
        reader.handle_key_event(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE)),
        ReaderKeyAction::Handled
    );
    assert_eq!(reader.current_page(), 1);

    assert_eq!(
        reader.handle_key_event(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL,)),
        ReaderKeyAction::Handled
    );
    {
        let character = '2';
        reader.handle_key_event(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
    }
    reader.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(reader.current_page(), 2);
    let jump = render_rows(&reader, 41, 3);
    assert!(jump[2].ends_with("[2/2]"));
}
