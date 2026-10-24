#![cfg(feature = "kitty-graphics")]

use libghostty_vt::{Terminal, kitty::graphics::PlacementIterator};

#[test]
fn enumerates_unplaced_raw_images_and_reuses_output_capacity() {
    let mut terminal = Terminal::new(8, 3).unwrap();
    terminal.set_kitty_image_storage_limit(1024 * 1024).unwrap();
    terminal.vt_write(b"\x1b_Ga=t,f=32,s=1,v=1,i=42;/wAA/w==\x1b\\");
    terminal.vt_write(b"\x1b_Ga=t,f=24,s=1,v=1,i=7;AAD/\x1b\\");

    let mut ids = Vec::with_capacity(1);
    {
        let graphics = terminal.kitty_graphics().unwrap();
        let mut iterator = PlacementIterator::new().unwrap();
        assert!(iterator.update(&graphics).unwrap().next().is_none());
        graphics.image_ids_into(&mut ids).unwrap();
    }
    assert_eq!(ids, [7, 42]);
    let allocation = ids.as_ptr();
    let capacity = ids.capacity();
    terminal
        .kitty_graphics()
        .unwrap()
        .image_ids_into(&mut ids)
        .unwrap();
    assert_eq!(ids, [7, 42]);
    assert_eq!(ids.as_ptr(), allocation);
    assert_eq!(ids.capacity(), capacity);

    terminal.vt_write(b"\x1b_Ga=d,d=I,i=42\x1b\\");
    terminal
        .kitty_graphics()
        .unwrap()
        .image_ids_into(&mut ids)
        .unwrap();
    assert_eq!(ids, [7]);
    assert_eq!(ids.as_ptr(), allocation);
    assert_eq!(ids.capacity(), capacity);

    terminal.vt_write(b"\x1b_Ga=d,d=I,i=7\x1b\\");
    terminal
        .kitty_graphics()
        .unwrap()
        .image_ids_into(&mut ids)
        .unwrap();
    assert!(ids.is_empty());
    assert_eq!(ids.as_ptr(), allocation);
    assert_eq!(ids.capacity(), capacity);
}

#[test]
fn empty_storage_clears_stale_ids_without_allocating() {
    let mut terminal = Terminal::new(8, 3).unwrap();
    terminal.set_kitty_image_storage_limit(1024).unwrap();
    let mut ids = Vec::new();
    terminal
        .kitty_graphics()
        .unwrap()
        .image_ids_into(&mut ids)
        .unwrap();
    assert!(ids.is_empty());
    assert_eq!(ids.capacity(), 0);
    ids.extend([7, 42]);
    let capacity = ids.capacity();
    terminal
        .kitty_graphics()
        .unwrap()
        .image_ids_into(&mut ids)
        .unwrap();
    assert!(ids.is_empty());
    assert_eq!(ids.capacity(), capacity);
}
