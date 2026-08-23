use super::*;
use pretty_assertions::assert_eq;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use std::fs;
use tempfile::tempdir;

fn render_line(reader: &Reader, width: u16) -> String {
    let area = Rect::new(0, 0, width, 1);
    let mut buffer = Buffer::empty(area);
    reader.render(area, &mut buffer);
    buffer
        .content
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>()
}

#[test]
fn pages_preserve_line_breaks_and_are_limited_to_35_graphemes() {
    let temp = tempdir().expect("temp directory");
    let home = temp.path().join("codex");
    let path = temp.path().join("book.txt");
    fs::write(&path, "第一行\n第二行，包含组合字符 e\u{301}。\n第三行").expect("write book");

    let reader = Reader::open(&path, &home, temp.path()).expect("open book");

    assert_eq!(
        reader.pages,
        vec!["第一行↵第二行，包含组合字符 e\u{301}。↵第三行".to_string()]
    );
    assert!(
        reader
            .pages
            .iter()
            .all(|page| { page.graphemes(true).count() <= PAGE_GRAPHEME_LIMIT })
    );
}

#[test]
fn progress_restores_only_for_the_same_file_content() {
    let temp = tempdir().expect("temp directory");
    let home = temp.path().join("codex");
    let path = temp.path().join("book.txt");
    fs::write(&path, "0123456789".repeat(7)).expect("write book");

    let mut reader = Reader::open(&path, &home, temp.path()).expect("open book");
    assert_eq!(reader.current_page(), 1);
    reader.current_page = 1;
    reader.save_progress();

    let restored = Reader::open(&path, &home, temp.path()).expect("reopen book");
    assert_eq!(restored.current_page(), 2);

    fs::write(&path, "changed content").expect("change book");
    let reset = Reader::open(&path, &home, temp.path()).expect("reopen changed book");
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
fn page_jump_and_left_aligned_rendering_stay_on_one_line() {
    let temp = tempdir().expect("temp directory");
    let path = temp.path().join("book.txt");
    fs::write(&path, "0123456789".repeat(7)).expect("write book");
    let mut reader = Reader::open(&path, temp.path(), temp.path()).expect("open book");

    let normal = render_line(&reader, 41);
    assert_eq!(normal, "[1/2] 01234567890123456789012345678901234");

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
    let jump = render_line(&reader, 41);
    assert_eq!(jump, "[2/2] 56789012345678901234567890123456789");

    insta::assert_snapshot!(
        format!("normal: {normal:?}\njump: {jump:?}"),
        @r###"
        normal: "[1/2] 01234567890123456789012345678901234"
        jump: "[2/2] 56789012345678901234567890123456789"
        "###
    );
}
