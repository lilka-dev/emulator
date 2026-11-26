extern crate self as lilka_emulator;
#[cfg(feature = "desktop")]
pub mod app;
#[cfg(feature = "desktop")]
pub mod audio_output;
#[cfg(feature = "desktop")]
pub mod controls;
pub mod firmware;
#[cfg(feature = "desktop")]
pub mod headless;
#[cfg(feature = "desktop")]
pub mod preferences;
#[cfg(feature = "desktop")]
pub mod profiles;
#[cfg(feature = "desktop")]
pub mod qemu_live;
#[cfg(feature = "desktop")]
pub mod radio;
pub mod storage;
#[cfg(feature = "web")]
pub mod web;
pub use lilka_board::{devices, hardware};

#[cfg(feature = "desktop")]
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
