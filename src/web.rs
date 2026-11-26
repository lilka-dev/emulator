//! Browser entry point.
//!
//! Emscripten runs `main` on a worker thread (PROXY_TO_PTHREAD), so blocking here
//! never stalls the page. QEMU's `main` hands over to [`lilka_web_main`], which
//! prepares the session the way the desktop launcher does and then runs the CPU
//! on this same thread. The page and the board share only the live protocol
//! buffer ([`lilka_board::live::Ipc`]) in wasm memory, plus [`lilka_web_request`]
//! for reset and stop.
use crate::{firmware, storage};
use anyhow::{Context, Result, bail, ensure};
use lilka_board::live::{Ipc, LAYOUT};
use std::{
    ffi::{CStr, CString, c_char, c_int},
    fs,
    path::PathBuf,
    sync::{
        OnceLock,
        atomic::{AtomicU32, Ordering},
    },
};

// The board bridge is reached only through QEMU's C calls, never from Rust.
extern crate lilka_qemu_board_bridge;

const FLASH: usize = 16 * 1024 * 1024;

unsafe extern "C" {
    /// system/main.c: initializes QEMU with `argv` and runs its main loop.
    fn lilka_cpu_run(argc: c_int, argv: *mut *mut c_char) -> c_int;
}

struct Options {
    firmware: Option<PathBuf>,
    app: Option<PathBuf>,
    rom_dir: PathBuf,
    sd: PathBuf,
    state: PathBuf,
    output: PathBuf,
    uart: PathBuf,
    resume: bool,
    paused: bool,
    threads: String,
}

fn parse(args: &[String]) -> Result<Options> {
    let mut options = Options {
        firmware: None,
        app: None,
        rom_dir: "/lilka/rom".into(),
        sd: "/lilka/sd".into(),
        state: "/lilka/session".into(),
        output: "/tmp/lilka-output".into(),
        uart: "/tmp/lilka-uart.log".into(),
        resume: false,
        paused: false,
        threads: "multi".into(),
    };
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--resume" => options.resume = true,
            "--paused" => options.paused = true,
            _ => {
                let value = args.next().with_context(|| format!("{flag} needs a value"))?;
                match flag.as_str() {
                    "--firmware" => options.firmware = Some(value.into()),
                    "--app" => options.app = Some(value.into()),
                    "--rom-dir" => options.rom_dir = value.into(),
                    "--sd" => options.sd = value.into(),
                    "--state" => options.state = value.into(),
                    "--output" => options.output = value.into(),
                    "--uart" => options.uart = value.into(),
                    "--threads" if matches!(value.as_str(), "single" | "multi") => {
                        options.threads = value.clone();
                    }
                    _ => bail!("Unknown option {flag} {value}"),
                }
            }
        }
    }
    Ok(options)
}

/// Writes the session flash (unless resuming) and returns QEMU's arguments.
fn prepare(options: &Options) -> Result<Vec<String>> {
    fs::create_dir_all(&options.state)?;
    fs::create_dir_all(&options.sd)?;
    fs::create_dir_all(&options.output)?;
    let flash_path = options.state.join("flash.bin");
    if !options.resume || !flash_path.is_file() {
        let base = options
            .firmware
            .as_ref()
            .context("A new session needs --firmware")?;
        let bytes = fs::read(base).with_context(|| format!("Cannot read {}", base.display()))?;
        ensure!(bytes.len() <= firmware::MAX_FLASH_BYTES, "Base firmware exceeds 16 MiB");
        let mut flash = firmware::normalize(&bytes)?;
        ensure!(
            flash.get(0x8000..0x8002) == Some(&[0xaa, 0x50]),
            "Expected merged flash with a partition table; add an app-only image as the app of a bootable base"
        );
        if let Some(app) = &options.app {
            let app = fs::read(app).with_context(|| format!("Cannot read {}", app.display()))?;
            ensure!(app.len() <= firmware::MAX_FLASH_BYTES, "Application exceeds 16 MiB");
            firmware::install_app(&mut flash, &app)?;
        }
        ensure!(flash.len() <= FLASH, "Firmware exceeds Lilka's 16 MiB flash");
        flash.resize(FLASH, 255);
        storage::atomic_write(&flash_path, &flash)?;
    }
    let mut flash = fs::read(&flash_path)?;
    ensure!(flash.len() == FLASH, "Incomplete session storage");
    // The browser has no network, so Wi-Fi stays off and Keira's saved emulator
    // network is cleared, as on a desktop profile without Wi-Fi.
    let compatibility = firmware::compat::prepare(&mut flash, false)?;
    let wifi_changed = compatibility.keira_known && firmware::nvs::configure_wifi(&mut flash, None)?;
    if compatibility.changed || wifi_changed {
        storage::atomic_write(&flash_path, &flash)?;
    }
    fs::write(
        options.output.join("compatibility.json"),
        serde_json::to_vec_pretty(&compatibility)?,
    )?;
    let live = options.output.join("live.ipc");
    Ipc::create(&live)?.request_running(!options.paused);

    let single = options.threads == "single";
    let mut variables = vec![
        ("LILKA_QEMU_BOARD_LIBRARY", "linked".to_owned()),
        ("LILKA_QEMU_OUTPUT_DIR", options.output.display().to_string()),
        ("LILKA_QEMU_LIVE_FILE", live.display().to_string()),
        ("LILKA_QEMU_SD_FOLDER", options.sd.display().to_string()),
    ];
    if compatibility.keira_vfs16 {
        variables.push(("LILKA_QEMU_KEIRA_VFS16", "1".into()));
    }
    // The same fast paths the desktop session enables.
    for key in [
        "LILKA_QEMU_PAGE_SPI",
        "LILKA_QEMU_CLOCKLESS_SPI",
        "LILKA_QEMU_CLOCKLESS_READS",
        "LILKA_QEMU_INLINE_SPI",
        "LILKA_QEMU_INLINE_SPI_HOST_CLOCK",
        "LILKA_QEMU_FILL16",
        "LILKA_QEMU_AUDIO_COMPLETIONS",
        "LILKA_QEMU_SPI_EVENTLESS",
        "LILKA_QEMU_SERVICE_CLOCK",
        "LILKA_QEMU_ROM_UDIV",
        "LILKA_QEMU_ROM_DIV_COMPAT",
        // The interpreter cannot always refill I2S buffers in time: play only
        // what the firmware wrote instead of looping stale buffers.
        "LILKA_QEMU_AUDIO_REFILL_WAIT",
    ] {
        variables.push((key, "1".into()));
    }
    if single {
        variables.push(("LILKA_QEMU_FAIR_RR", "1".into()));
    }
    for (key, value) in variables {
        // SAFETY: QEMU and the board threads do not exist yet.
        unsafe { std::env::set_var(key, value) };
    }

    let mut args: Vec<String> = vec!["lilka-emulator".into()];
    if options.paused {
        args.push("-S".into());
    }
    args.extend(
        [
            "-L",
            &options.rom_dir.display().to_string(),
            "-machine",
            "esp32s3",
            "-accel",
            if single { "tcg,thread=single" } else { "tcg,thread=multi" },
            "-m",
            "16M",
            "-drive",
            &format!(
                "file={},if=mtd,format=raw",
                flash_path.display().to_string().replace(',', ",,")
            ),
            "-display",
            "none",
            "-serial",
            &format!("file:{}", options.uart.display()),
            "-monitor",
            "none",
            "-nic",
            "none",
            "-seed",
            "1234",
            "-global",
            "driver=ssi_psram,property=is_octal,value=true",
        ]
        .map(String::from),
    );
    Ok(args)
}

/// Called by QEMU's `main` in the browser build, on Emscripten's main worker.
#[unsafe(no_mangle)]
pub extern "C" fn lilka_web_main(argc: c_int, argv: *const *const c_char) -> c_int {
    let args: Vec<String> = (1..argc.max(0) as usize)
        // SAFETY: Emscripten passes argc NUL-terminated strings.
        .map(|i| unsafe { CStr::from_ptr(*argv.add(i)) }.to_string_lossy().into_owned())
        .collect();
    let qemu = match parse(&args).and_then(|options| prepare(&options)) {
        Ok(qemu) => qemu,
        Err(error) => {
            eprintln!("Lilka: {error:#}");
            let _ = Ipc::create(std::path::Path::new("failed")).map(|live| live.set_status(4));
            return 1;
        }
    };
    let qemu: Vec<CString> = qemu
        .into_iter()
        .map(|arg| CString::new(arg).expect("arguments contain no NUL"))
        .collect();
    let mut pointers: Vec<_> = qemu.iter().map(|arg| arg.as_ptr().cast_mut()).collect();
    pointers.push(std::ptr::null_mut());
    // SAFETY: argv holds argc strings and a NULL, all alive until QEMU returns.
    unsafe { lilka_cpu_run(qemu.len() as c_int, pointers.as_mut_ptr()) }
}

/// Address of the live protocol buffer in wasm memory.
#[unsafe(no_mangle)]
pub extern "C" fn lilka_web_live() -> *const u8 {
    Ipc::base()
}

/// The live protocol's byte offsets as a JSON object (static NUL-terminated string).
#[unsafe(no_mangle)]
pub extern "C" fn lilka_web_layout() -> *const c_char {
    static JSON: OnceLock<CString> = OnceLock::new();
    JSON.get_or_init(|| {
        let map: serde_json::Map<_, _> = LAYOUT
            .iter()
            .map(|&(name, value)| (name.to_owned(), value.into()))
            .collect();
        CString::new(serde_json::Value::Object(map).to_string()).unwrap()
    })
    .as_ptr()
}

static REQUEST: AtomicU32 = AtomicU32::new(0);
/// Asks the CPU for a reset (1) or a clean stop (2). Safe from any thread: QEMU
/// picks the request up from its 5 ms live-control timer.
#[unsafe(no_mangle)]
pub extern "C" fn lilka_web_request(request: u32) {
    if matches!(request, 1 | 2) {
        REQUEST.store(request, Ordering::Release);
    }
}
/// Called by QEMU's live-control timer with the BQL held.
#[unsafe(no_mangle)]
pub extern "C" fn lilka_web_take_request() -> u32 {
    REQUEST.swap(0, Ordering::AcqRel)
}
