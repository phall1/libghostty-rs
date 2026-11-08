//! The PNG hook is process-global; isolate concurrent installation from tests
//! that intentionally install arbitrary unsafe hooks.
#![cfg(all(feature = "kitty-graphics", feature = "png", not(miri)))]

use libghostty_vt::{Terminal, kitty::graphics::install_rust_png_decoder};

#[test]
fn built_in_decoder_can_be_installed_while_other_terminals_decode() {
    let workers: Vec<_> = (0..8)
        .map(|_| {
            std::thread::spawn(|| {
                let mut terminal = Terminal::new(10, 3).unwrap();
                for _ in 0..32 {
                    install_rust_png_decoder().unwrap();
                    terminal.vt_write(
                        b"\x1b_Ga=t,f=100,i=1,q=2;\
                    iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAA\
                    DUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==\
                    \x1b\\",
                    );
                    let graphics = terminal.kitty_graphics().unwrap().unwrap();
                    let image = graphics.image(1).expect("PNG accepted");
                    assert_eq!(image.data().unwrap().unwrap(), [255, 0, 0, 255]);
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
}
