use super::*;

#[test]
fn continuation_restores_split_sequences_and_long_strings_resume_at_ground() {
    for prefix in [b"\x1b_Gm=1;AAAA".as_slice(), b"\x1b]2;title", b"\x1bP+q544", b"\x1b[31", b"\xf0\x9f"] {
        let mut screen = Terminal::new(80, 24, 1 << 20);
        screen.write(prefix);
        assert_eq!(screen.continuation().unwrap(), prefix);
    }
    let mut screen = Terminal::new(80, 24, 1 << 20);
    screen.write(b"\x1b_Gm=1;");
    screen.write(&vec![b'A'; CONTINUATION_MAX_BYTES]);
    assert!(screen.continuation().is_none());
    assert_eq!(screen.write_until_ground(b"AAAA"), None);
    assert_eq!(screen.write_until_ground(b"AA\x1b\\\x1b_Gm=1;BBBB"), Some(4));
    assert_eq!(screen.continuation(), Some(Vec::new()));
}

#[test]
fn display_changes_report_to_subscribers_and_queries_use_the_latest_facts() {
    let mut t = Terminal::new(80, 24, 0);
    t.write(b"\x1b[?2031h");
    t.take_responses(); // Enabling reporting may itself report the current scheme.
    t.set_display(Some([1, 2, 3]), Some([255; 3]), Some([4, 5, 6]), Some((9, 18)));
    assert_eq!(t.take_responses(), b"\x1b[?997;2n", "dark to light");
    t.set_display(None, Some([0; 3]), None, None);
    assert_eq!(t.take_responses(), b"\x1b[?997;1n", "light to dark");

    // Mode 2031 also tells subscribers to re-query colors within the same scheme.
    t.set_display(Some([7, 8, 9]), None, None, None);
    assert_eq!(t.take_responses(), b"\x1b[?997;1n", "foreground changed");
    t.set_display(None, None, Some([10, 11, 12]), None);
    assert_eq!(t.take_responses(), b"\x1b[?997;1n", "cursor changed");
    t.set_display(Some([7, 8, 9]), Some([0; 3]), Some([10, 11, 12]), Some((10, 20)));
    t.set_display(None, None, None, None);
    assert!(t.take_responses().is_empty(), "repeated colors and cell size are silent");

    t.write(b"\x1b]10;?\x07\x1b]11;?\x07\x1b]12;?\x07\x1b[?996n\x1b[16t");
    assert_eq!(
        t.take_responses(),
        b"\x1b]10;rgb:0707/0808/0909\x07\x1b]11;rgb:0000/0000/0000\x07\x1b]12;rgb:0a0a/0b0b/0c0c\x07\x1b[?997;1n\x1b[6;20;10t"
    );
}

#[test]
fn display_changes_do_not_report_without_a_subscription() {
    let mut t = Terminal::new(80, 24, 0);
    t.set_display(None, Some([255; 3]), None, None);
    assert!(t.take_responses().is_empty());
    t.write(b"\x1b[?2031h\x1b[?2031l");
    t.take_responses();
    t.set_display(None, Some([0; 3]), None, None);
    assert!(t.take_responses().is_empty(), "disabled subscription stays silent");
}

#[test]
fn a_repaint_into_a_dirty_viewer_turns_off_modes_the_screen_no_longer_has() {
    let mut viewer = Terminal::new(40, 10, 0);
    viewer.write(b"\x1b[?1000h\x1b[?1006h\x1b[?2004h\x1b[?1004h");
    let screen = Terminal::new(40, 10, 0);
    viewer.write(&screen.paint(0));
    for mode in [1000, 1006, 2004, 1004] {
        assert!(!viewer.dec_mode(mode), "mode {mode} left on");
    }
    let mut mouse = Terminal::new(40, 10, 0);
    mouse.write(b"\x1b[?1000h");
    viewer.write(&mouse.paint(0));
    assert!(viewer.dec_mode(1000), "and the screen's own modes come back");
}

#[test]
fn parses_reads_and_round_trips_a_snapshot() {
    let mut t = Terminal::new(20, 3, 1 << 20);
    for i in 0..10 {
        t.write(format!("line {i}\r\n").as_bytes());
    }
    let text = t.plain_text();
    assert!(text.contains("line 0") && text.contains("line 9"), "{text:?}");

    // Snapshot taken in the middle of an escape sequence finishes it after restore.
    t.write(b"\x1b[3");
    let snap = t.snapshot().unwrap();
    let mut back = Terminal::restore(&snap, 1 << 20).unwrap();
    back.write(b"1mred\x1b[0m");
    assert_eq!(back.size(), (20, 3));
    let text = back.plain_text();
    assert!(text.contains("line 0") && text.contains("red"), "{text:?}");
    assert!(!text.contains("1mred"), "{text:?}");

    t.write(b"m\x1b[?1049h");
    assert!(t.alt_screen());
    assert!(Terminal::restore(b"garbage", 1 << 20).is_none());
    // A snapshot an older Ghostty wrote (here: cut short, or with a foreign
    // header) is refused, never half-restored; callers then start a fresh screen.
    assert!(Terminal::restore(&snap[..snap.len() / 2], 1 << 20).is_none());
    assert!(Terminal::restore(&snap[..4], 1 << 20).is_none());
    let mut foreign = snap;
    foreign[0] ^= 0xff;
    assert!(Terminal::restore(&foreign, 1 << 20).is_none());
}

#[test]
fn a_painted_terminal_looks_the_same_on_a_fresh_one() {
    let mut t = Terminal::new(30, 4, 1 << 20);
    for i in 0..8 {
        t.write(format!("\x1b[31mline {i}\x1b[0m\r\n").as_bytes());
    }
    t.write(b"\x1b]2;my title\x07\x1b[?2026h\x1b[?1004hprompt> ");
    let mut fresh = Terminal::new(30, 4, 1 << 20);
    fresh.write(&t.paint(3));
    let screen = |t: &Terminal| t.plain_text().lines().rev().take(4).map(String::from).collect::<Vec<_>>();
    assert_eq!(screen(&fresh), screen(&t));
    assert!(fresh.plain_text().contains("line 2"), "{:?}", fresh.plain_text());
    assert!(!fresh.plain_text().contains("line 1\n"), "history beyond 3 lines");
    assert_eq!(fresh.title(), "my title");
    assert!(fresh.dec_mode(1004));
    assert!(!fresh.dec_mode(2026), "synchronized output must not be replayed");

    let mut alt = Terminal::new(30, 4, 1 << 20);
    alt.write(b"main\r\n\x1b[?1049h\x1b[2;3Hfull screen");
    let mut fresh = Terminal::new(30, 4, 1 << 20);
    fresh.write(&alt.paint(10));
    assert!(fresh.alt_screen());
    assert!(fresh.plain_text().contains("full screen"));
}

#[test]
fn repaint_uses_the_active_screen_not_stale_alternate_screen_modes() {
    let mut t = Terminal::new(30, 4, 1 << 20);
    t.write(b"main\r\n\x1b[?47h\x1b[?1049hALT\x1b[?1049lDONE");
    assert!(!t.alt_screen());
    let mut fresh = Terminal::new(30, 4, 1 << 20);
    fresh.write(&t.paint(10));
    assert!(!fresh.alt_screen());
    assert_eq!(fresh.screen_rows(), t.screen_rows());

    for enter in [47, 1047, 1049] {
        for exit in [None, Some(47), Some(1047), Some(1049)] {
            let mut t = Terminal::new(30, 4, 1 << 20);
            t.write(format!("main\r\n\x1b[?{enter}h\x1b[2;3HALT").as_bytes());
            if let Some(exit) = exit {
                t.write(format!("\x1b[?{exit}lDONE").as_bytes());
            }
            let paint = t.paint(10);
            for viewer_mode in [None, Some(47), Some(1047), Some(1049)] {
                let mut viewer = Terminal::new(30, 4, 1 << 20);
                if let Some(mode) = viewer_mode {
                    viewer.write(format!("old main\x1b[?{mode}hOLD ALT").as_bytes());
                }
                viewer.write(&paint);
                let case = (enter, exit, viewer_mode);
                assert_eq!(viewer.alt_screen(), t.alt_screen(), "{case:?}");
                assert_eq!(viewer.screen_rows(), t.screen_rows(), "{case:?}");
                assert!(!viewer.dec_mode(47), "{case:?}");
                assert!(!viewer.dec_mode(1047), "{case:?}");
                assert_eq!(viewer.dec_mode(1049), t.alt_screen(), "{case:?}");
            }
        }
    }
}

#[test]
fn program_status_reports_and_title_changes_reach_us() {
    let mut t = Terminal::new(80, 24, 1 << 20);
    t.write(b"\x1b]7501;state=working:app=claude-code:progress=40\x1b\\");
    t.write(b"\x1b]7501;state=blocked:kind=permission:id=a/b:msg=QXBwbHk/\x07");
    t.write(b"\x1b]7501;state=clear\x1b\\\x1b]7501;state=bogus\x1b\\");
    let reports = t.take_status_reports();
    assert_eq!(reports.len(), 3, "{reports:?}");
    assert_eq!((reports[0].state, reports[0].progress, reports[0].app.as_str()), ("working", Some(40), "claude-code"));
    assert_eq!(
        (reports[1].kind, reports[1].id.as_str(), reports[1].message.as_str()),
        (Some("permission"), "a/b", "Apply?")
    );
    assert_eq!(reports[2].state, "clear");
    // A program asking whether we support the protocol gets an answer.
    t.write(b"\x1b]7501;?\x1b\\");
    assert!(String::from_utf8_lossy(&t.take_responses()).starts_with("\x1b]7501;?"));

    assert!(!t.take_title_changed());
    t.write(b"\x1b]2;hi\x07");
    assert!(t.take_title_changed());
}

#[test]
fn a_new_shell_prompt_arrives_in_order_with_status_reports() {
    let mut t = Terminal::new(80, 24, 1 << 20);
    t.write(b"\x1b]7501;state=working\x1b\\\x1b]133;A\x07\x1b]7501;state=done\x1b\\");
    let states: Vec<_> = t.take_status_reports().iter().map(|r| r.state).collect();
    assert_eq!(states, ["working", "prompt", "done"]);
    // Only a prompt start counts: not the input, output or end of a command, nor plain text.
    t.write(b"\x1b]133;B\x07\x1b]133;C\x07\x1b]133;D;0\x07ls\r\n");
    assert!(t.take_status_reports().is_empty());
    t.write(b"\x1b]133;A;k=s\x1b\\\x1b]133;A\x07");
    assert_eq!(t.take_status_reports().len(), 2, "a repeated prompt start is harmless but reported");
}

#[test]
fn wheel_notches_follow_the_programs_mouse_mode_and_rows_keep_their_width() {
    let mut t = Terminal::new(20, 4, 1 << 20);
    assert!(t.wheel(true).is_empty(), "no mouse reports asked for");
    t.write(b"\x1b[?1049h\x1b[?1000h\x1b[?1006h");
    assert_eq!(String::from_utf8_lossy(&t.wheel(true)), "\x1b[<64;11;3M");
    assert_eq!(String::from_utf8_lossy(&t.wheel(false)), "\x1b[<65;11;3M");
    t.write(b"hello\r\nworld  \x1b[2;19H\xe2\x94\x82");
    let rows = t.screen_rows();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0], format!("{:<20}", "hello"));
    assert!(rows[1].starts_with("world") && rows[1].chars().nth(18) == Some('│'), "{:?}", rows[1]);
    assert!(rows.iter().all(|r| r.chars().count() == 20));
}

#[test]
fn answers_queries_like_ghostty_with_viewer_facts() {
    let mut t = Terminal::new(80, 24, 1 << 20);
    t.set_xtversion("WezTerm 20240203");
    t.set_display(Some([1, 2, 3]), Some([0xff, 0xff, 0xff]), None, Some((9, 18)));
    let ask = |t: &mut Terminal, q: &[u8]| {
        t.write(q);
        String::from_utf8_lossy(&t.take_responses()).into_owned()
    };
    assert_eq!(ask(&mut t, b"\x1b[c"), "\x1b[?62;22c");
    assert_eq!(ask(&mut t, b"\x1b[>c"), "\x1b[>1;10;0c");
    assert!(ask(&mut t, b"\x1b[>q").contains("WezTerm 20240203"));
    assert_eq!(ask(&mut t, b"\x1b[3;5H\x1b[6n"), "\x1b[3;5R");
    assert!(ask(&mut t, b"\x1b]11;?\x07").contains("rgb:ffff/ffff/ffff"));
    assert!(ask(&mut t, b"\x1b]10;?\x07").contains("rgb:0101/0202/0303"));
    assert_eq!(ask(&mut t, b"\x1b[?996n"), "\x1b[?997;2n", "light background");
    assert_eq!(ask(&mut t, b"\x1b[16t"), "\x1b[6;18;9t");
    assert_eq!(ask(&mut t, b"\x1b[18t"), "\x1b[8;24;80t");
    assert_eq!(ask(&mut t, b"\x1b[?2004$p"), "\x1b[?2004;2$y");
    assert_eq!(ask(&mut t, b"\x1b[?u"), "\x1b[?0u");
}

#[test]
fn grapheme_clustering_is_on_like_ghostty() {
    let mut t = Terminal::new(10, 2, 0);
    t.write(b"\x1b[?2027$p");
    assert_eq!(String::from_utf8_lossy(&t.take_responses()), "\x1b[?2027;1$y");
    t.write("\u{270d}\u{fe0f}!".as_bytes());
    assert_eq!(t.plain_text().trim_end(), "\u{270d}\u{fe0f}!");
    t.write(b"\x1bc");
    assert!(t.dec_mode(2027), "a reset keeps it on");
}

// Regression tests for the Ghostty fixes of 2026-10-27 (pin v1.4.0). They would
// crash, or corrupt the terminal, on the previous pin.

#[test]
fn narrowing_a_65535_column_terminal_does_not_overflow_the_row_count() {
    // GHSA-jqr7-69mm-v35h: a page this wide, narrowed, holds more than 65535 rows.
    let mut t = Terminal::new(u16::MAX, 24, 1 << 20);
    t.write(b"wide");
    t.resize(4, 24);
    assert_eq!(t.size(), (4, 24));
    t.write(b"\r\nok");
    assert!(t.plain_text().contains("ok"));

    // The same through a snapshot, the way a crafted one reaches the decoder.
    let wide = Terminal::new(u16::MAX, 24, 1 << 20);
    let mut back = Terminal::restore(&wide.snapshot().unwrap(), 1 << 20).unwrap();
    back.resize(3, 24);
    assert_eq!(back.size(), (3, 24));
}

#[test]
fn line_feeds_on_a_one_row_terminal_with_a_scrollback_limit_do_not_free_the_cursor_page() {
    // GHSA-qjfx-r533-6vx3: reflow a long hyperlinked line wider, then fill the page.
    let mut t = Terminal::new(10, 1, 1 << 12);
    t.write(b"\x1b]8;;http://a.example\x1b\\");
    t.write(&[b'x'; 200]);
    t.resize(200, 1);
    for _ in 0..4000 {
        t.write(b"\n");
    }
    t.write(b"\x1b]8;;\x1b\\done");
    assert!(t.plain_text().contains("done"));
}

#[test]
fn clearing_the_screen_with_a_styled_hyperlinked_cursor_and_a_tiny_scrollback_is_safe() {
    // GHSA-w9q4-5jvv-mcv4: CSI 22 J / CSI 2 J may prune the cursor's page.
    for clear in [b"\x1b[22J".as_slice(), b"\x1b[2J"] {
        let mut t = Terminal::new(80, 24, 1);
        t.write(b"\x1b[1;31m\x1b]8;;http://a.example\x1b\\");
        for i in 0..200 {
            t.write(format!("line {i}\r\n").as_bytes());
            t.write(clear);
            t.write(b"\x1b[4;32mtext");
        }
        t.write(b"\x1b[0m\x1b]8;;\x1b\\\x1b[Hend");
        assert!(t.plain_text().contains("end"));
    }
}

#[test]
fn kitty_clipboard_alias_updates_stay_bounded() {
    // GHSA-3r5j-c6rj-xq2v: an open OSC 5522 write transaction updating one alias.
    // We install no clipboard_write callback, so Ghostty rejects the transaction (with an error reply); no crash.
    let mut t = Terminal::new(80, 24, 0);
    t.write(b"\x1b]5522;type=write\x1b\\");
    // The rejection is the whole story: no transaction exists for the aliases to grow.
    assert_eq!(String::from_utf8_lossy(&t.take_responses()), "\x1b]5522;type=write:status=ENOSYS\x1b\\");
    for _ in 0..20_000 {
        t.write(b"\x1b]5522;type=walias:mime=text/plain;dGV4dC9wbGFpbg==\x1b\\");
    }
    assert!(t.take_responses().is_empty(), "alias updates outside a transaction are ignored");
    t.write(b"ok");
    assert!(t.plain_text().contains("ok"));
}

#[test]
fn write_until_ground_stops_right_after_a_sequence_split_at_any_byte() {
    let sequences: [&[u8]; 12] = [
        b"\x1b[38;2;1;2;3m",
        b"\x1b]2;title\x07",
        b"\x1b]2;title\x1b\\",
        b"\x1bP+q544e\x1b\\",
        b"\x1b_Ga=T,f=100;iVBORw0KGgo=\x1b\\",
        b"\x1b]7501;state=working:progress=40\x1b\\",
        b"\x1b]133;A\x07",
        b"\x1bc",
        b"\x1b7",
        "é".as_bytes(),
        "€".as_bytes(),
        "😀".as_bytes(),
    ];
    for seq in sequences {
        // Text right after the sequence, or the next sequence: neither may be consumed.
        for tail in [b"Zq".as_slice(), b"\x1b[1mZ"] {
            let mut whole = Terminal::new(20, 4, 1 << 20);
            whole.write(seq);
            whole.write(tail);
            let (reports, responses) = (whole.take_status_reports(), whole.take_responses());
            // The core's effects fire on this engine: OSC 7501, a prompt start (133 A), a reset (RIS).
            let expected: &[&str] = match seq {
                s if s.starts_with(b"\x1b]7501") => &["working"],
                s if s.starts_with(b"\x1b]133") => &["prompt"],
                b"\x1bc" => &["clear"],
                _ => &[],
            };
            assert_eq!(reports.iter().map(|r| r.state).collect::<Vec<_>>(), expected, "{seq:?}");
            for k in 1..seq.len() {
                let mut t = Terminal::new(20, 4, 1 << 20);
                t.write(&seq[..k]);
                let rest = [&seq[k..], tail].concat();
                assert_eq!(t.write_until_ground(&rest), Some(seq.len() - k), "{seq:?} split at {k}, tail {tail:?}");
                assert_eq!(t.continuation(), Some(Vec::new()), "{seq:?} split at {k}");
                assert!(!t.plain_text().contains('Z'), "{seq:?} split at {k}: wrote past ground");
                t.write(&rest[seq.len() - k..]);
                assert_eq!(t.plain_text(), whole.plain_text(), "{seq:?} split at {k}");
                assert_eq!(t.title(), whole.title(), "{seq:?} split at {k}");
                assert_eq!(t.take_status_reports(), reports, "{seq:?} split at {k}");
                assert_eq!(t.take_responses(), responses, "{seq:?} split at {k}");
            }
        }
    }
}

#[test]
fn write_until_ground_on_empty_input_at_ground_and_on_input_that_never_gets_there() {
    let mut t = Terminal::new(20, 4, 1 << 20);
    // Already at ground: it stops at once and consumes nothing, not even text.
    assert_eq!(t.write_until_ground(b""), Some(0));
    assert_eq!(t.write_until_ground(b"text"), Some(0));
    assert!(!t.plain_text().contains("text"));
    // Inside a sequence: an empty write is no progress, the rest is all consumed.
    t.write(b"\x1b]2;ti");
    assert_eq!(t.write_until_ground(b""), None);
    assert_eq!(t.write_until_ground(b"tl"), None);
    assert_eq!(t.continuation().unwrap(), b"\x1b]2;titl");
    assert_eq!(t.write_until_ground(b"e\x1b"), None, "the ESC of an ST is not ground yet");
    assert_eq!(t.write_until_ground(b"\\\x1b[31"), Some(1), "the first ground, not the end of the write");
    assert_eq!(t.title(), "title");
    assert_eq!(t.continuation(), Some(Vec::new()));
    // A UTF-8 character split three ways.
    t.write(b"\xf0");
    assert_eq!(t.write_until_ground(b"\x9f"), None);
    assert_eq!(t.write_until_ground(b"\x98"), None);
    assert_eq!(t.write_until_ground(b"\x80x"), Some(1));
    assert!(t.plain_text().contains('😀') && !t.plain_text().contains('x'));
}

#[test]
fn write_until_ground_finds_ground_at_any_offset_of_a_huge_write() {
    // Ghostty parses large writes in 4096-byte chunks: put the ST on and around their edges.
    for payload in [0, 1, 4093, 4094, 4095, 4096, 8190, 8191, 1 << 20] {
        let mut t = Terminal::new(80, 24, 1 << 20);
        t.write(b"\x1b_Gm=1;");
        let mut input = vec![b'A'; payload];
        input.extend_from_slice(b"\x1b\\\x1b_Gm=1;BBBB");
        assert_eq!(t.write_until_ground(&input), Some(payload + 2), "payload {payload}");
        assert_eq!(t.continuation(), Some(Vec::new()), "payload {payload}");
    }
    // No ground anywhere in a huge write: all of it is consumed.
    let mut t = Terminal::new(80, 24, 1 << 20);
    t.write(b"\x1b]2;");
    assert_eq!(t.write_until_ground(&vec![b'x'; 1 << 20]), None);
    assert_eq!(t.write_until_ground(b"\x07after"), Some(1));
}

#[test]
fn a_snapshot_written_by_the_previous_ghostty_pin_still_restores() {
    // Ghostty's complete fixture, byte for byte the same at the previous pin (1e5cdb8)
    // and written by its encoder. v1.4.0 kept wire version 1 and only added optional
    // records, so an old core handing over passes its screens on.
    let old: Vec<u8> = include_str!("../../../vendor/ghostty/src/terminal/snapshot/testdata/complete-v1.hex")
        .lines()
        .flat_map(|line| line.split('#').next().unwrap_or_default().split_whitespace())
        .map(|byte| u8::from_str_radix(byte, 16).unwrap())
        .collect();
    assert_eq!(&old[..10], b"GHOSTSNP\x01\x00");
    let mut t = Terminal::restore(&old, 1 << 20).expect("an old snapshot restores");
    assert_eq!(t.size(), (2, 3));
    assert!(t.alt_screen());
    t.write(b"\x1b[?1049l");
    assert!(!t.alt_screen());
    let text = t.plain_text();
    assert!(text.contains('C') && text.contains('D') && text.contains('E'), "{text:?}");
    // It snapshots again (in the new format) and keeps working.
    let mut again = Terminal::restore(&t.snapshot().unwrap(), 1 << 20).unwrap();
    assert_eq!(again.plain_text(), text);
    again.write(b"\r\nok");
    assert!(again.plain_text().contains("ok"));

    // Damaged: cut short anywhere it is refused; with any byte flipped it is
    // refused or usable, never a crash.
    for cut in 0..old.len() {
        assert!(Terminal::restore(&old[..cut], 1 << 20).is_none(), "cut at {cut}");
    }
    for i in 0..old.len() {
        let mut bad = old.clone();
        bad[i] ^= 0x5a;
        if let Some(mut t) = Terminal::restore(&bad, 1 << 20) {
            t.write(b"\r\nok");
            assert!(t.plain_text().contains("ok"), "byte {i} flipped");
        }
    }
}
