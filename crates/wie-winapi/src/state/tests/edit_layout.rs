//! Pure multiline-EDIT layout tests for `layout_visible_lines` (wrap, per-logical-line segments, scroll clamping, alignment).

// ── Task 2.2: pure multiline-EDIT layout helper (`layout_visible_lines`).

/// A row's (text, y) as a plain pair for compact assertions.
fn row_pair(row: &crate::user32::controls::VisibleSegment) -> (&str, i32) {
    (row.text.as_str(), row.y)
}

#[test]
fn test_edit_layout_wrap_off_one_row_per_logical_line() {
    // "ab\ncd\nef" without wrap: 3 logical lines, one row each, at
    // y = line × line_height, carrying the whole-text char offsets the
    // selection/caret math needs.
    let rows = crate::user32::controls::layout_visible_lines(
        "ab\ncd\nef",
        80,
        16,
        0,
        false,
        0,
        &mut |_ch: char| 8_i32,
    );
    let shown: Vec<(&str, i32)> = rows.iter().map(row_pair).collect();
    assert_eq!(shown, [("ab", 0), ("cd", 16), ("ef", 32)]);
    assert_eq!((rows[0].char_start, rows[0].char_end), (0, 2));
    assert_eq!((rows[1].char_start, rows[1].char_end), (3, 5));
    assert_eq!((rows[2].char_start, rows[2].char_end), (6, 8));
    assert!(
        rows.iter().all(|r| r.x == 0),
        "left-aligned rows start at 0"
    );
}

#[test]
fn test_edit_layout_wrap_splits_long_line_at_width() {
    // Wrap on, 8 px/char, 32 px column → 4 chars per visual row: "abcdef"
    // becomes "abcd" at y=0 and "ef" at y=16, with contiguous char offsets.
    let rows = crate::user32::controls::layout_visible_lines(
        "abcdef",
        32,
        16,
        0,
        true,
        0,
        &mut |_ch: char| 8_i32,
    );
    let shown: Vec<(&str, i32)> = rows.iter().map(row_pair).collect();
    assert_eq!(shown, [("abcd", 0), ("ef", 16)]);
    assert_eq!((rows[0].char_start, rows[0].char_end), (0, 4));
    assert_eq!((rows[1].char_start, rows[1].char_end), (4, 6));
}

#[test]
fn test_edit_layout_wrap_applies_per_logical_line() {
    // The wrap column resets between logical lines: "ab" fits untouched and
    // "cdef" wraps to 3 chars + 1 in the same 24 px (8 px/char) column.
    let rows = crate::user32::controls::layout_visible_lines(
        "ab\ncdef",
        24,
        16,
        0,
        true,
        0,
        &mut |_ch: char| 8_i32,
    );
    let shown: Vec<(&str, i32)> = rows.iter().map(row_pair).collect();
    assert_eq!(shown, [("ab", 0), ("cde", 16), ("f", 32)]);
    assert_eq!((rows[1].char_start, rows[1].char_end), (3, 6));
    assert_eq!((rows[2].char_start, rows[2].char_end), (6, 7));
}

#[test]
fn test_edit_layout_blank_line_occupies_a_row() {
    // "ab\n\ncd" splits into 3 logical lines; the empty middle line still
    // occupies its vertical slot so the following text lands at y=32.
    let rows = crate::user32::controls::layout_visible_lines(
        "ab\n\ncd",
        80,
        16,
        0,
        false,
        0,
        &mut |_ch: char| 8_i32,
    );
    let shown: Vec<(&str, i32)> = rows.iter().map(row_pair).collect();
    assert_eq!(shown, [("ab", 0), ("", 16), ("cd", 32)]);
}

#[test]
fn test_edit_layout_first_visible_skips_rows_and_rebases_y() {
    let rows = crate::user32::controls::layout_visible_lines(
        "ab\ncd\nef",
        80,
        16,
        1,
        false,
        0,
        &mut |_ch: char| 8_i32,
    );
    let shown: Vec<(&str, i32)> = rows.iter().map(row_pair).collect();
    assert_eq!(shown, [("cd", 0), ("ef", 16)]);
}

#[test]
fn test_edit_layout_scroll_clamps_past_last_row() {
    // first_visible beyond the last row clamps to the last row: only "ef"
    // remains, at the top.
    let rows = crate::user32::controls::layout_visible_lines(
        "ab\ncd\nef",
        80,
        16,
        5,
        false,
        0,
        &mut |_ch: char| 8_i32,
    );
    let shown: Vec<(&str, i32)> = rows.iter().map(row_pair).collect();
    assert_eq!(shown, [("ef", 0)]);

    // The clamp counts VISUAL rows: wrapped lines make the last visual row
    // later than the last logical line.
    let wrapped = crate::user32::controls::layout_visible_lines(
        "abcdef",
        32,
        16,
        9,
        true,
        0,
        &mut |_ch: char| 8_i32,
    );
    let wrapped_shown: Vec<(&str, i32)> = wrapped.iter().map(row_pair).collect();
    assert_eq!(wrapped_shown, [("ef", 0)]);
}

#[test]
fn test_edit_layout_alignment_offsets_row_x() {
    // "ab" = 16 px in a 40 px column: ES_CENTER → x=12, ES_RIGHT → x=24.
    let centered = crate::user32::controls::layout_visible_lines(
        "ab",
        40,
        16,
        0,
        false,
        0x1,
        &mut |_ch: char| 8_i32,
    );
    assert_eq!(centered[0].x, 12);
    let right = crate::user32::controls::layout_visible_lines(
        "ab",
        40,
        16,
        0,
        false,
        0x2,
        &mut |_ch: char| 8_i32,
    );
    assert_eq!(right[0].x, 24);
    // A row wider than the column still starts at the left edge.
    let overwide = crate::user32::controls::layout_visible_lines(
        "abcdefgh",
        40,
        16,
        0,
        false,
        0x2,
        &mut |_ch: char| 8_i32,
    );
    assert_eq!(overwide[0].x, 0);
}

#[test]
fn test_edit_layout_empty_text_single_empty_row() {
    let rows =
        crate::user32::controls::layout_visible_lines("", 80, 16, 0, true, 0, &mut |_ch: char| {
            8_i32
        });
    assert_eq!(rows.len(), 1);
    assert_eq!(row_pair(&rows[0]), ("", 0));
}
