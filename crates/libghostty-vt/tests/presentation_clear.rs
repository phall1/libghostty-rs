#![cfg(not(miri))]

use libghostty_vt::{
    Terminal,
    snapshot::Decoder,
    style::{PaletteIndex, StyleColor},
    terminal::{Point, PointCoordinate},
};
use std::cell::RefCell;

#[test]
fn clear_preserves_pending_dcs_and_emits_no_effects() {
    let responses = RefCell::new(Vec::<u8>::new());
    let mut terminal = Terminal::new(20, 2).unwrap();
    terminal.set_scrollback_max_lines(Some(100)).unwrap();
    terminal
        .on_pty_write(|_, bytes| responses.borrow_mut().extend_from_slice(bytes))
        .unwrap();
    terminal.vt_write(b"old\r\nhistory\r\nvisible\x1b[1m\x1bP$q");
    assert!(terminal.scrollback_rows().unwrap() > 0);
    terminal.clear_presentation();
    assert_eq!(responses.borrow().len(), 0);
    assert_eq!(terminal.scrollback_rows().unwrap(), 0);
    terminal.vt_write(b"m\x1b\\");
    assert_eq!(&*responses.borrow(), b"\x1bP1$r0;1m\x1b\\");
}

#[test]
fn clear_preserves_parser_restored_from_snapshot() {
    let mut source = Terminal::new(40, 12).unwrap();
    source.set_continuation_max_bytes(64 * 1024).unwrap();
    source.vt_write(b"hello\x1b[3");
    let mut encoded = Vec::new();
    source.encode_snapshot(&mut encoded).unwrap();
    let mut terminal = Decoder::new_buf(&encoded).unwrap().decode().unwrap();
    terminal.clear_presentation();
    terminal.vt_write(b"1mX");
    let cell = terminal
        .grid_ref(Point::Active(PointCoordinate { x: 0, y: 0 }))
        .unwrap();
    assert_eq!(cell.cell().unwrap().codepoint().unwrap(), u32::from(b'X'));
    assert_eq!(
        cell.style().unwrap().fg_color,
        StyleColor::Palette(PaletteIndex::RED)
    );
}
