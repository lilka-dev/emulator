extern crate self as lilka_emulator;
pub mod app;
pub mod audio_output;
pub mod controls;
pub mod firmware;
pub mod headless;
pub mod preferences;
pub mod profiles;
pub mod qemu_live;
pub mod radio;
pub mod storage;
pub use lilka_board::{devices, hardware};

#[unsafe(no_mangle)]
pub extern "C" fn lilka_desktop_main() -> i32 {
    match std::panic::catch_unwind(app::run) {
        Ok(Ok(())) => 0,
        Ok(Err(error)) => {
            eprintln!("{error:#}");
            1
        }
        Err(_) => 1,
    }
}
