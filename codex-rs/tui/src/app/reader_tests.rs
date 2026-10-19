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
            let mut line = String::new();
            let mut skip_columns = 0usize;
            for cell in row {
                if skip_columns > 0 {
                    skip_columns -= 1;
                    continue;
                }
                let symbol = cell.symbol();
                line.push_str(symbol);
                skip_columns = UnicodeWidthStr::width(symbol).saturating_sub(1);
            }
            line.trim_end().to_string()
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

    let status_width = page_status_width(reader.page_count(), reader.search_status_width());
    let page_lines = page_line_ranges(
        &reader.text,
        reader.pages[0].start_byte_offset,
        reader.layout_width,
        reader.layout_height,
        status_width,
    )
    .into_iter()
    .map(|range| &reader.text[range])
    .collect::<Vec<_>>();
    assert_eq!(page_lines, vec!["  第一行", "第二行  ", "第三行"]);
    let rows = render_rows(&reader, 20, 3);
    assert_eq!(rows, vec!["  第一行", "第二行", "第三行 [1/1]"]);
}

#[test]
fn pagination_wraps_wide_characters_by_terminal_columns() {
    let text = "A界BCDEF";
    let pages = paginate(text, 12, 2);
    let status_width = page_status_width(pages.len(), 0);
    let rendered_pages = pages
        .iter()
        .map(|page| {
            page_line_ranges(text, page.start_byte_offset, 12, 2, status_width)
                .into_iter()
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let reconstructed = rendered_pages
        .iter()
        .flatten()
        .map(|range| &text[range.clone()])
        .collect::<String>();

    assert_eq!(reconstructed, text);
    assert!(rendered_pages.iter().all(|lines| {
        lines.iter().enumerate().all(|(index, range)| {
            UnicodeWidthStr::width(&text[range.clone()]) <= if index == 1 { 6 } else { 12 }
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

    let mut reader = Reader::open(&path, &home, temp.path()).expect("open book");
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

#[test]
fn reader_width_caps_long_lines_and_alignment_places_the_column() {
    let temp = tempdir().expect("temp directory");
    let path = temp.path().join("book.txt");
    fs::write(&path, format!("first row\n{}\nthird row", "x".repeat(80))).expect("write book");
    let mut reader =
        Reader::open_with_layout(&path, temp.path(), temp.path(), 120, 3).expect("open book");
    reader.set_configured_width(Some(40));

    assert_eq!(reader.layout_width, 40);
    assert_eq!(reader.page_count(), 2);
    let first_page_lines = page_line_ranges(
        &reader.text,
        reader.pages[0].start_byte_offset,
        reader.layout_width,
        reader.layout_height,
        page_status_width(reader.page_count(), reader.search_status_width()),
    );
    assert_eq!(&reader.text[first_page_lines[1].clone()], "x".repeat(40));
    assert_eq!(render_rows(&reader, 120, 3)[0], "first row");

    reader.set_alignment(TuiReaderAlignment::Center);
    assert_eq!(
        render_rows(&reader, 120, 3)[0],
        format!("{}first row", " ".repeat(40))
    );

    reader.set_alignment(TuiReaderAlignment::Right);
    assert_eq!(
        render_rows(&reader, 120, 3)[0],
        format!("{}first row", " ".repeat(80))
    );
}

#[test]
fn decodes_bom_marked_utf8_utf16_and_utf32_text() {
    let expected = "Hello 世界 😀";
    let mut utf8 = vec![0xef, 0xbb, 0xbf];
    utf8.extend_from_slice(expected.as_bytes());
    assert_eq!(decode_text(&utf8), expected);

    let mut utf16_le = vec![0xff, 0xfe];
    let mut utf16_be = vec![0xfe, 0xff];
    let mut utf32_le = vec![0xff, 0xfe, 0x00, 0x00];
    let mut utf32_be = vec![0x00, 0x00, 0xfe, 0xff];
    let mut utf16_buffer = [0; 2];
    for character in expected.chars() {
        for unit in character.encode_utf16(&mut utf16_buffer).iter() {
            utf16_le.extend_from_slice(&unit.to_le_bytes());
            utf16_be.extend_from_slice(&unit.to_be_bytes());
        }
        let codepoint = character as u32;
        utf32_le.extend_from_slice(&codepoint.to_le_bytes());
        utf32_be.extend_from_slice(&codepoint.to_be_bytes());
    }

    assert_eq!(decode_text(&utf16_le), expected);
    assert_eq!(decode_text(&utf16_be), expected);
    assert_eq!(decode_text(&utf32_le), expected);
    assert_eq!(decode_text(&utf32_be), expected);
    assert_eq!(decode_text(&[0xff]), "�");
}

#[test]
fn search_is_case_insensitive_and_moves_between_matches() {
    let temp = tempdir().expect("temp directory");
    let path = temp.path().join("book.txt");
    fs::write(&path, "top\nneedle first\nmiddle\nNEEDLE second\nbottom\n").expect("write book");
    let mut reader =
        Reader::open_with_layout(&path, temp.path(), temp.path(), 40, 1).expect("open book");

    let ctrl_f = KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL);
    assert_eq!(reader.handle_key_event(ctrl_f), ReaderKeyAction::Handled);
    for character in "needle".chars() {
        reader.handle_key_event(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
    }
    assert!(render_rows(&reader, 40, 1)[0].contains("[find: needle_]"));
    reader.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(reader.search_matches.len(), 2);
    assert_eq!(reader.current_page(), 2);
    let first = render_rows(&reader, 40, 1)[0].clone();
    assert!(first.contains("needle first"));
    assert!(first.contains("[find 1/2]"));

    reader.handle_key_event(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    assert_eq!(reader.current_page(), 4);
    let second = render_rows(&reader, 40, 1)[0].clone();
    assert!(second.contains("NEEDLE second"));
    assert!(second.contains("[find 2/2]"));

    reader.handle_key_event(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT));
    assert_eq!(reader.current_page(), 2);
    assert!(render_rows(&reader, 40, 1)[0].contains("[find 1/2]"));

    reader.handle_key_event(ctrl_f);
    reader.handle_key_event(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    assert_eq!(
        reader.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        ReaderKeyAction::Handled
    );
    assert_eq!(
        reader.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        ReaderKeyAction::Close
    );
}

#[test]
fn search_reflows_pages_to_reserve_status_width() {
    let temp = tempdir().expect("temp directory");
    let path = temp.path().join("book.txt");
    fs::write(&path, "x".repeat(200)).expect("write book");
    let mut reader =
        Reader::open_with_layout(&path, temp.path(), temp.path(), 20, 3).expect("open book");
    let pages_without_search_status = reader.page_count();

    reader.handle_key_event(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
    reader.handle_key_event(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE));
    reader.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert!(reader.page_count() > pages_without_search_status);
    assert!(render_rows(&reader, 20, 3)[2].contains("[no matches]"));
}

#[test]
fn search_limits_stored_matches_for_repetitive_large_files() {
    let text = "needle ".repeat(MAX_SEARCH_MATCHES + 2);
    let (matches, truncated) = find_matches(&text, "needle");

    assert_eq!(matches.len(), MAX_SEARCH_MATCHES);
    assert!(truncated);
}

#[test]
fn recent_reader_files_are_ordered_and_bounded() {
    let temp = tempdir().expect("temp directory");
    let home = temp.path().join("codex");
    let first_path = temp.path().join("first.txt");
    let second_path = temp.path().join("second.txt");
    fs::write(&first_path, "first").expect("write first");
    fs::write(&second_path, "second").expect("write second");

    for path in [&first_path, &second_path, &first_path] {
        let mut reader =
            Reader::open_with_layout(path, &home, temp.path(), 40, 3).expect("open reader");
        reader.save_progress();
    }

    assert_eq!(
        recent_reader_paths(&home.join(PROGRESS_FILE_NAME)),
        vec![
            fs::canonicalize(&first_path).expect("canonicalize first"),
            fs::canonicalize(&second_path).expect("canonicalize second"),
        ]
    );
}

#[test]
fn progress_writes_are_throttled_and_dropped_progress_is_flushed() {
    let temp = tempdir().expect("temp directory");
    let home = temp.path().join("codex");
    let path = temp.path().join("book.txt");
    fs::write(&path, "0123456789".repeat(60)).expect("write book");
    let mut reader = Reader::open_with_layout(&path, &home, temp.path(), 20, 3).expect("open book");
    let progress_key = reader.progress_key.clone();
    reader.save_progress();

    let now = Instant::now();
    reader.last_progress_save = now;
    reader.current_page = 1;
    reader.save_progress_if_due_at(now + Duration::from_millis(500));
    assert_eq!(
        read_progress_store(&home.join(PROGRESS_FILE_NAME)).entries[&progress_key].page,
        1
    );

    reader.save_progress_if_due_at(now + Duration::from_secs(1));
    assert_eq!(
        read_progress_store(&home.join(PROGRESS_FILE_NAME)).entries[&progress_key].page,
        2
    );

    reader.current_page = 2;
    reader.progress_dirty = true;
    drop(reader);
    assert_eq!(
        read_progress_store(&home.join(PROGRESS_FILE_NAME)).entries[&progress_key].page,
        3
    );
}
