#![cfg(all(feature = "kitty-graphics", not(miri)))]

use libghostty_vt::{
    Terminal,
    kitty::graphics::{ImageIterator, PlacementIterator},
};

#[test]
fn placeholder_default_prefers_lowest_external_placement() {
    let mut terminal = Terminal::new(10, 5).unwrap();
    terminal.vt_write(b"\x1b_Ga=T,f=32,s=1,v=1,i=42,p=9,U=1,c=1,r=1,q=2;/wAA/w==\x1b\\");
    terminal.vt_write(b"\x1b_Ga=p,i=42,p=2,U=1,c=1,r=1,q=2;\x1b\\");
    terminal.vt_write(b"\x1b_Ga=p,i=42,U=1,c=1,r=1,q=2;\x1b\\");
    let graphics = terminal.kitty_graphics().unwrap().unwrap();
    let mut iterator = PlacementIterator::new().unwrap();
    let mut placements = iterator.update(&graphics).unwrap();
    let mut defaults = Vec::new();
    let mut external = 0;
    let mut anonymous = 0;
    while let Some(placement) = placements.next() {
        if placement.is_placeholder_default().unwrap() {
            defaults.push(placement.placement_id().unwrap());
        }
        if placement.is_external().unwrap() {
            external += 1;
        } else {
            anonymous += 1;
        }
    }
    assert_eq!(defaults, [2]);
    assert_eq!((external, anonymous), (2, 1));
}

#[test]
fn enumerates_unplaced_images_and_reuses_output_capacity() {
    let mut terminal = Terminal::new(8, 3).unwrap();
    terminal.set_kitty_image_storage_limit(1024 * 1024).unwrap();
    terminal.vt_write(b"\x1b_Ga=t,f=32,s=1,v=1,i=42;/wAA/w==\x1b\\");
    terminal.vt_write(b"\x1b_Ga=t,f=24,s=1,v=1,i=7;AAD/\x1b\\");
    let mut ids = Vec::with_capacity(1);
    {
        let graphics = terminal.kitty_graphics().unwrap().unwrap();
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
        .unwrap()
        .image_ids_into(&mut ids)
        .unwrap();
    assert_eq!(ids, [7, 42]);
    assert_eq!(ids.as_ptr(), allocation);
    terminal.vt_write(b"\x1b_Ga=d,d=I,i=42\x1b\\");
    terminal
        .kitty_graphics()
        .unwrap()
        .unwrap()
        .image_ids_into(&mut ids)
        .unwrap();
    assert_eq!(ids, [7]);
    terminal.vt_write(b"\x1b_Ga=d,d=I,i=7\x1b\\");
    terminal
        .kitty_graphics()
        .unwrap()
        .unwrap()
        .image_ids_into(&mut ids)
        .unwrap();
    assert_eq!(ids.len(), 0);
    assert_eq!(ids.as_ptr(), allocation);
    assert_eq!(ids.capacity(), capacity);
}

#[test]
fn empty_storage_clears_stale_ids_without_allocating_output() {
    let mut terminal = Terminal::new(8, 3).unwrap();
    terminal.set_kitty_image_storage_limit(1024).unwrap();
    let mut ids = Vec::new();
    terminal
        .kitty_graphics()
        .unwrap()
        .unwrap()
        .image_ids_into(&mut ids)
        .unwrap();
    assert_eq!(ids.len(), 0);
    assert_eq!(ids.capacity(), 0);
    ids.extend([7, 42]);
    let capacity = ids.capacity();
    terminal
        .kitty_graphics()
        .unwrap()
        .unwrap()
        .image_ids_into(&mut ids)
        .unwrap();
    assert_eq!(ids.len(), 0);
    assert_eq!(ids.capacity(), capacity);
}

#[test]
fn image_iterator_can_be_reused_and_images_outlive_iteration() {
    let mut terminal = Terminal::new(8, 3).unwrap();
    let mut iterator = ImageIterator::new().unwrap();
    let graphics = terminal.kitty_graphics().unwrap().unwrap();
    assert!(iterator.update(&graphics).unwrap().next().is_none());

    terminal.vt_write(b"\x1b_Ga=t,f=32,s=1,v=1,i=42;/wAA/w==\x1b\\");
    terminal.vt_write(b"\x1b_Ga=t,f=24,s=1,v=1,i=7;AAD/\x1b\\");
    {
        let graphics = terminal.kitty_graphics().unwrap().unwrap();
        let mut images: Vec<_> = iterator.update(&graphics).unwrap().collect();
        images.sort_by_key(|image| image.id().unwrap());
        assert_eq!(
            images.iter().map(|i| i.id().unwrap()).collect::<Vec<_>>(),
            [7, 42]
        );
        assert_eq!(images[0].width().unwrap(), 1);
        assert_eq!(images[1].data().unwrap().unwrap(), [255, 0, 0, 255]);
    }
    terminal.vt_write(b"\x1b_Ga=d,d=I,i=42\x1b\\");
    let graphics = terminal.kitty_graphics().unwrap().unwrap();
    let mut images = iterator.update(&graphics).unwrap();
    assert_eq!(images.next().unwrap().id().unwrap(), 7);
    assert!(images.next().is_none());
    assert!(images.next().is_none());
}
