use anyhow::{Context, Result, ensure};
pub use lilka_board::live::{HEIGHT, Ipc, Stats, WIDTH};
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone)]
pub struct Config {
    pub binary: PathBuf,
    pub bridge: PathBuf,
    pub rom_dir: PathBuf,
    pub firmware: PathBuf,
    pub sd: PathBuf,
    pub state_dir: PathBuf,
    pub wifi_ssid: Option<String>,
}
impl Config {
    fn validate_radio(&self) -> Result<()> {
        if let Some(ssid) = &self.wifi_ssid {
            ensure!(
                (1..=32).contains(&ssid.len()) && !ssid.contains('\0'),
                "Назва Wi-Fi повинна містити від 1 до 32 байтів без NUL"
            );
        }
        Ok(())
    }
}
pub struct Session {
    pub ipc: Arc<Ipc>,
    pub directory: PathBuf,
    config: Config,
    cpu: Option<EmbeddedCpu>,
    _storage_lock: File,
}
impl Session {
    pub fn start(config: Config) -> Result<Self> {
        Self::start_with_app(config, None, false)
    }
    pub fn start_app(config: Config, app: &Path) -> Result<Self> {
        Self::start_with_app(config, Some(app), false)
    }
    pub fn start_desktop(
        config: Config,
        app: Option<&Path>,
        resume: Option<&Path>,
    ) -> Result<Self> {
        if let Some(directory) = resume {
            Self::resume_with_pause(config, directory, true)
        } else {
            Self::start_with_app(config, app, true)
        }
    }
    pub fn resume(config: Config, directory: &Path) -> Result<Self> {
        Self::resume_with_pause(config, directory, false)
    }
    fn resume_with_pause(mut config: Config, directory: &Path, paused: bool) -> Result<Self> {
        config.validate_radio()?;
        let directory = directory.canonicalize()?;
        let storage_lock = Self::lock_storage(&directory)?;
        ensure!(
            fs::read(directory.join("closed-v1")).ok().as_deref()
                == Some(b"lilka-qemu-storage-v1\n"),
            "Session has no clean shutdown record; it may still have a running CPU: {}",
            directory.display()
        );
        ensure!(
            directory.join("flash.bin").is_file()
                && fs::metadata(directory.join("flash.bin"))?.len() == 16 * 1024 * 1024
                && directory.join("sd-folder.json").is_file(),
            "Incomplete session storage"
        );
        let stored_sd: PathBuf =
            serde_json::from_slice(&fs::read(directory.join("sd-folder.json"))?)?;
        config.sd = if config.sd.is_dir() {
            config.sd.canonicalize()?
        } else {
            stored_sd
        };
        ensure!(config.sd.is_dir(), "SD folder is no longer available");
        for path in [&config.binary, &config.bridge] {
            ensure!(path.is_file(), "Expected regular file: {}", path.display());
        }
        if directory.join("live.ipc").try_exists()? {
            fs::remove_file(directory.join("live.ipc"))?;
        }
        fs::write(
            directory.join("sd-folder.json"),
            serde_json::to_vec(&config.sd)?,
        )?;
        let ipc = Arc::new(Ipc::create(&directory.join("live.ipc"))?);
        let mut session = Self {
            ipc,
            directory,
            config,
            cpu: None,
            _storage_lock: storage_lock,
        };
        session.ipc.request_running(!paused);
        session.spawn(paused)?;
        Ok(session)
    }
    fn lock_storage(directory: &Path) -> Result<File> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("session.lock"))?;
        file.try_lock().with_context(|| {
            format!("Session storage is already in use: {}", directory.display())
        })?;
        Ok(file)
    }
    fn start_with_app(mut config: Config, app: Option<&Path>, paused: bool) -> Result<Self> {
        config.validate_radio()?;
        for path in [&config.binary, &config.bridge, &config.firmware] {
            ensure!(path.is_file(), "Expected regular file: {}", path.display());
        }
        ensure!(config.sd.is_dir(), "Оберіть теку SD-карти");
        ensure!(
            fs::metadata(&config.firmware)?.len() <= crate::firmware::MAX_FLASH_BYTES as u64,
            "Base firmware exceeds 16 MiB"
        );
        let mut flash = crate::firmware::normalize(&fs::read(&config.firmware)?)?;
        ensure!(
            flash.get(0x8000..0x8002) == Some(&[0xaa, 0x50]),
            "Expected merged flash with a partition table; use --app APP.bin with a bootable base for an app-only image"
        );
        if let Some(app) = app {
            ensure!(app.is_file(), "Expected app file: {}", app.display());
            ensure!(
                fs::metadata(app)?.len() <= crate::firmware::MAX_FLASH_BYTES as u64,
                "Application exceeds 16 MiB"
            );
            crate::firmware::install_app(&mut flash, &fs::read(app)?)?;
        }
        ensure!(
            flash.len() <= 16 * 1024 * 1024,
            "Firmware exceeds Lilka's 16 MiB flash"
        );
        flash.resize(16 * 1024 * 1024, 255);
        config.sd = config.sd.canonicalize()?;
        fs::create_dir_all(&config.state_dir)?;
        let directory = config.state_dir.join(format!(
            "session-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir(&directory)?;
        let directory = directory.canonicalize()?;
        let storage_lock = Self::lock_storage(&directory)?;
        fs::write(directory.join("flash.bin"), flash)?;
        fs::write(
            directory.join("sd-folder.json"),
            serde_json::to_vec(&config.sd)?,
        )?;
        let ipc = Arc::new(Ipc::create(&directory.join("live.ipc"))?);
        let mut session = Self {
            ipc,
            directory,
            config,
            cpu: None,
            _storage_lock: storage_lock,
        };
        session.ipc.request_running(!paused);
        session.spawn(paused)?;
        Ok(session)
    }
    fn spawn(&mut self, paused: bool) -> Result<()> {
        let flash_path = self.directory.join("flash.bin");
        let mut flash = fs::read(&flash_path)?;
        let mut compatibility =
            crate::firmware::compat::prepare(&mut flash, self.config.wifi_ssid.is_some())?;
        if compatibility.keira_known {
            compatibility.wifi_settings_changed =
                crate::firmware::nvs::configure_wifi(&mut flash, self.config.wifi_ssid.as_deref())?;
        }
        if compatibility.changed || compatibility.wifi_settings_changed {
            crate::storage::atomic_write(&flash_path, &flash)?;
        }
        fs::write(
            self.directory.join("compatibility.json"),
            serde_json::to_vec_pretty(&compatibility)?,
        )?;
        let preference = match std::env::var("LILKA_QEMU_TCG_THREADS") {
            Ok(value) => value,
            Err(std::env::VarError::NotPresent) => "auto".to_owned(),
            Err(error) => return Err(error).context("Invalid LILKA_QEMU_TCG_THREADS"),
        };
        let single = match preference.as_str() {
            "auto" => {
                std::env::var_os("LILKA_QEMU_STRICT_CLOCK").is_none()
                    && std::thread::available_parallelism().is_ok_and(|n| n.get() == 1)
            }
            "single" => true,
            "multi" => false,
            _ => anyhow::bail!("LILKA_QEMU_TCG_THREADS must be auto, single or multi"),
        };
        let mut cmd = Command::new(self.config.binary.canonicalize()?);
        if let Some(port) = std::env::var_os("LILKA_QEMU_GDB_PORT") {
            let port: u16 = port.to_str().context("Invalid debugger port")?.parse()?;
            ensure!(port != 0, "Debugger port must be nonzero");
            cmd.args(["-gdb", &format!("tcp:127.0.0.1:{port}")]);
        }
        if paused {
            cmd.arg("-S");
        }
        cmd.arg("-L")
            .arg(self.config.rom_dir.canonicalize()?)
            .args([
                "-machine",
                "esp32s3",
                "-accel",
                if single {
                    "tcg,thread=single"
                } else {
                    "tcg,thread=multi"
                },
                "-m",
                "16M",
                "-drive",
            ])
            .arg(format!(
                "file={},if=mtd,format=raw",
                self.directory
                    .join("flash.bin")
                    .display()
                    .to_string()
                    .replace(',', ",,")
            ))
            .args(["-display", "none", "-serial"])
            .arg(format!(
                "file:{}",
                self.directory.join("uart.log").display()
            ))
            .args([
                "-monitor",
                "none",
                "-nic",
                "none",
                "-seed",
                "1234",
                "-global",
                "driver=ssi_psram,property=is_octal,value=true",
            ]);
        for name in [
            "LILKA_QEMU_SD_FOLDER",
            "LILKA_QEMU_SD_IMAGE",
            "LILKA_QEMU_SD_FILE_BACKED",
            "LILKA_QEMU_INPUTS",
            "LILKA_QEMU_STOP_NS",
            "LILKA_QEMU_CLOCK_RATE",
            "LILKA_QEMU_SERVICE_CLOCK",
            "LILKA_QEMU_FAIR_RR",
            "LILKA_QEMU_LAZY_INTERRUPT_ACK",
            "LILKA_QEMU_SPI_EVENTLESS",
            "LILKA_QEMU_AUDIO_PREFETCH",
            "LILKA_QEMU_CLOCKLESS_PIO",
            "LILKA_QEMU_CHAIN_WINDOWS",
            "LILKA_QEMU_CAPTURE_PCM",
            "LILKA_QEMU_LEDC_REGISTERS",
            "LILKA_QEMU_ROM_UDIV",
            "LILKA_QEMU_ROM_DIV_COMPAT",
            "LD_PRELOAD",
            "LILKA_SAMPLES",
        ] {
            cmd.env_remove(name);
        }
        cmd.env(
            "LILKA_QEMU_BOARD_LIBRARY",
            self.config.bridge.canonicalize()?,
        )
        .env("LILKA_QEMU_OUTPUT_DIR", &self.directory)
        .env("LILKA_QEMU_LIVE_FILE", self.directory.join("live.ipc"));
        cmd.env("LILKA_QEMU_SD_FOLDER", &self.config.sd);
        cmd.env_remove("LILKA_QEMU_WIFI_SSID");
        cmd.env_remove("LILKA_QEMU_KEIRA_VFS16");
        if compatibility.keira_vfs16 {
            cmd.env("LILKA_QEMU_KEIRA_VFS16", "1");
        }
        if let Some(ssid) = &self.config.wifi_ssid {
            cmd.env("LILKA_QEMU_WIFI_SSID", ssid);
        }
        for key in [
            "LILKA_QEMU_PAGE_SPI",
            "LILKA_QEMU_CLOCKLESS_SPI",
            "LILKA_QEMU_CLOCKLESS_READS",
            "LILKA_QEMU_INLINE_SPI",
            "LILKA_QEMU_INLINE_SPI_HOST_CLOCK",
            "LILKA_QEMU_FILL16",
            "LILKA_QEMU_AUDIO_COMPLETIONS",
            "LILKA_QEMU_SPI_EVENTLESS",
        ] {
            cmd.env(key, "1");
        }
        if std::env::var_os("LILKA_QEMU_STRICT_CLOCK").is_none() {
            cmd.env("LILKA_QEMU_SERVICE_CLOCK", "1");
            if single {
                cmd.env("LILKA_QEMU_FAIR_RR", "1");
            }
        }
        if std::env::var_os("LILKA_QEMU_STRICT_ROM").is_none() {
            cmd.env("LILKA_QEMU_ROM_UDIV", "1")
                .env("LILKA_QEMU_ROM_DIV_COMPAT", "1");
        }
        let marker = self.directory.join("closed-v1");
        match fs::remove_file(marker) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        match fs::remove_file(self.directory.join("sd-folder-flushed")) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        self.cpu = Some(EmbeddedCpu::start(&cmd)?);
        Ok(())
    }
    pub fn exited(&mut self) -> Result<Option<ExitStatus>> {
        self.cpu.as_mut().context("No CPU runtime")?.status()
    }
    pub fn pause(&self, paused: bool) {
        self.ipc.request_running(!paused);
    }
    pub fn reset(&mut self) -> Result<()> {
        self.ipc.release_inputs();
        self.ipc.request_running(true);
        self.request_guest_reset()
    }
    pub fn request_guest_reset(&mut self) -> Result<()> {
        let cpu = self.cpu.as_ref().context("CPU is not running")?;
        // SAFETY: The exported wrapper serializes this request with QEMU's BQL.
        unsafe { (cpu.reset)() };
        Ok(())
    }
    pub fn stop(&mut self) -> Result<()> {
        let Some(cpu) = self.cpu.as_mut() else {
            return Ok(());
        };
        // SAFETY: The wrapper only submits a thread-safe QEMU shutdown request.
        unsafe { (cpu.shutdown)() };
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = cpu.status()? {
                break status;
            }
            ensure!(
                Instant::now() < deadline,
                "CPU shutdown timed out; storage remains unclean"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        self.cpu.take();
        ensure!(status.success(), "CPU shutdown was not clean: {status}");
        if self.directory.join("sd-folder.json").is_file() {
            ensure!(
                fs::read(self.directory.join("sd-folder-flushed"))
                    .ok()
                    .as_deref()
                    == Some(b"ok\n"),
                "SD folder writes were not flushed successfully"
            );
        }
        fs::write(self.directory.join("closed-v1"), b"lilka-qemu-storage-v1\n")?;
        Ok(())
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

struct EmbeddedCpu {
    thread: Option<std::thread::JoinHandle<i32>>,
    code: Option<i32>,
    shutdown: unsafe extern "C" fn(),
    reset: unsafe extern "C" fn(),
}
impl EmbeddedCpu {
    fn start(command: &Command) -> Result<Self> {
        use std::{
            ffi::{CString, c_char},
            os::unix::ffi::OsStrExt,
            sync::atomic::{AtomicBool, Ordering},
        };
        static STARTED: AtomicBool = AtomicBool::new(false);
        ensure!(
            !STARTED.swap(true, Ordering::SeqCst),
            "QEMU requires a fresh process for another session"
        );
        let library = libloading::os::unix::Library::this();
        // SAFETY: These symbols belong to the final executable and remain loaded
        // for the process lifetime. Their signatures are defined in system/main.c.
        let (run, ready, shutdown, reset) = unsafe {
            (
                *library.get::<unsafe extern "C" fn(i32, *mut *mut c_char) -> i32>(
                    b"lilka_cpu_run\0",
                )?,
                *library.get::<unsafe extern "C" fn() -> bool>(b"lilka_cpu_ready\0")?,
                *library.get::<unsafe extern "C" fn()>(b"lilka_cpu_shutdown\0")?,
                *library.get::<unsafe extern "C" fn()>(b"lilka_cpu_reset\0")?,
            )
        };
        let args: Vec<CString> = std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(|arg| CString::new(arg.as_bytes()))
            .collect::<std::result::Result<_, _>>()?;
        // SAFETY: Startup is called before Iced, audio and QEMU create threads.
        // The desktop executes itself with --profile before reaching this path.
        for (key, value) in command.get_envs() {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
        let thread = std::thread::Builder::new()
            .name("lilka-cpu".into())
            .spawn(move || {
                let mut pointers: Vec<_> = args.iter().map(|arg| arg.as_ptr().cast_mut()).collect();
                pointers.push(std::ptr::null_mut());
                // SAFETY: CString storage outlives the complete QEMU execution; argv
                // has argc entries followed by NULL and QEMU owns this worker thread.
                unsafe { run(args.len() as i32, pointers.as_mut_ptr()) }
            })?;
        let deadline = Instant::now() + Duration::from_secs(15);
        // SAFETY: The wrapper reads an atomic flag and never accesses VM state.
        while !unsafe { ready() } {
            ensure!(!thread.is_finished(), "CPU initialization failed");
            ensure!(Instant::now() < deadline, "CPU initialization timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(Self {
            thread: Some(thread),
            code: None,
            shutdown,
            reset,
        })
    }
    fn status(&mut self) -> Result<Option<ExitStatus>> {
        use std::os::unix::process::ExitStatusExt;
        if self.code.is_none()
            && self
                .thread
                .as_ref()
                .is_some_and(|thread| thread.is_finished())
        {
            self.code = Some(
                self.thread
                    .take()
                    .unwrap()
                    .join()
                    .map_err(|_| anyhow::anyhow!("CPU worker panicked"))?,
            );
        }
        Ok(self.code.map(|code| ExitStatus::from_raw(code << 8)))
    }
}
