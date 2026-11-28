mod audio;
mod launcher;
mod terminal;
use anyhow::{Context, Result, ensure};
use audio::AudioWorker;
use iced::{
    Color, Element, Event, Length, Size, Subscription, Task, Theme, keyboard,
    widget::{button, column, container, image, row, text},
    window,
};
use lilka_emulator::{
    audio_output::AudioOutput,
    controls::BUTTONS,
    qemu_live::{Config, HEIGHT, Session, WIDTH},
};
use std::{
    path::PathBuf,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
enum Message {
    Tick,
    Ready,
    Event(Event),
    Tap(usize),
    Pause,
    Reset,
    Close(window::Id),
    Terminal(terminal::Message),
}
struct App {
    session: Session,
    terminal: terminal::Terminal,
    audio: Option<AudioOutput>,
    worker: Option<AudioWorker>,
    frame: image::Handle,
    sequence: u64,
    rgba: Vec<u8>,
    held: [bool; 10],
    pulse: [Option<Instant>; 10],
    paused: bool,
    error: Option<String>,
    window_ready: bool,
    keys: [String; 10],
}
impl App {
    fn new(session: Session) -> Self {
        let audio = if std::env::var_os("LILKA_QEMU_AUDIO_NULL").is_some() {
            AudioOutput::null()
        } else {
            AudioOutput::new()
        }
        .map_err(|e| eprintln!("Audio output: {e}"))
        .ok();
        let worker = Some(AudioWorker::start(
            session.ipc.clone(),
            audio.as_ref().map(AudioOutput::sink),
        ));
        Self {
            terminal: terminal::Terminal::new(),
            session,
            audio,
            worker,
            frame: image::Handle::from_rgba(
                WIDTH as u32,
                HEIGHT as u32,
                vec![0; WIDTH * HEIGHT * 4],
            ),
            sequence: 0,
            rgba: Vec::new(),
            held: [false; 10],
            pulse: [None; 10],
            paused: false,
            error: None,
            window_ready: false,
            keys: lilka_emulator::preferences::Settings::default().keys,
        }
    }
    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            iced::time::every(Duration::from_nanos(1_000_000_000 / 30)).map(|_| Message::Tick),
            iced::event::listen_with(|event, status, _| {
                if matches!(event, Event::Keyboard(keyboard::Event::KeyPressed { .. }))
                    && status == iced::event::Status::Captured
                {
                    None
                } else {
                    Some(Message::Event(event))
                }
            }),
            window::close_requests().map(Message::Close),
            if self.window_ready {
                Subscription::none()
            } else {
                window::frames().map(|_| Message::Ready)
            },
        ])
    }
    fn sync_keys(&mut self) {
        for (i, (_, pin)) in BUTTONS.iter().enumerate() {
            if self.pulse[i].is_some_and(|at| Instant::now() >= at) {
                self.pulse[i] = None;
            }
            self.session
                .ipc
                .set_pin(*pin, !(self.held[i] || self.pulse[i].is_some()));
        }
    }
    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Close(id) => {
                if self.terminal.window == Some(id) {
                    self.terminal.window = None;
                    return window::close(id);
                }
                return iced::exit();
            }
            Message::Terminal(message) => return self.terminal.update(message),
            Message::Pause => {
                self.paused = !self.paused;
                self.session.pause(self.paused);
                if self.paused {
                    self.held.fill(false);
                    self.pulse.fill(None);
                    self.session.ipc.release_inputs();
                    if let Some(audio) = &self.audio {
                        audio.clear();
                    }
                }
            }
            Message::Reset => {
                self.worker.take();
                if let Some(audio) = &self.audio {
                    audio.clear();
                }
                let mut pending = Vec::new();
                self.session.ipc.drain_audio(&mut pending);
                let reset = self.session.reset();
                if let Err(error) = reset {
                    self.error = Some(error.to_string());
                } else {
                    self.error = None;
                    self.paused = false;
                    self.held.fill(false);
                    self.pulse.fill(None);
                }
                self.worker = Some(AudioWorker::start(
                    self.session.ipc.clone(),
                    self.audio.as_ref().map(AudioOutput::sink),
                ));
            }
            Message::Tap(i) if !self.paused => {
                self.pulse[i] = Some(Instant::now() + Duration::from_millis(120));
            }
            Message::Event(Event::Keyboard(keyboard::Event::KeyPressed { key, .. })) => {
                if key == keyboard::Key::Named(keyboard::key::Named::F5) {
                    return self.update(Message::Reset);
                }
                if key == keyboard::Key::Character("p".into()) {
                    return self.update(Message::Pause);
                }
                if !self.paused {
                    if let Some(i) = self
                        .keys
                        .iter()
                        .position(|binding| binding == &lilka_emulator::controls::key_name(&key))
                    {
                        self.held[i] = true;
                    }
                }
            }
            Message::Event(Event::Keyboard(keyboard::Event::KeyReleased { key, .. })) => {
                if let Some(i) = self
                    .keys
                    .iter()
                    .position(|binding| binding == &lilka_emulator::controls::key_name(&key))
                {
                    self.held[i] = false;
                }
            }
            Message::Event(Event::Window(window::Event::Unfocused)) => {
                self.held.fill(false);
                self.pulse.fill(None);
            }
            Message::Ready => {
                self.window_ready = true;
                if self.terminal.mode == lilka_emulator::preferences::TerminalMode::Window {
                    return self.terminal.update(terminal::Message::Open);
                }
            }
            Message::Tick => {
                self.terminal.tick(&self.session);
                if self.window_ready && !self.paused && !self.session.ipc.wants_running() {
                    self.session.pause(false);
                }
                self.session.ipc.publish_playback(
                    self.audio
                        .as_ref()
                        .map_or(0, |a| a.played.load(Ordering::Relaxed)),
                    self.audio
                        .as_ref()
                        .map_or(0, |a| a.underruns.load(Ordering::Relaxed)),
                    self.worker
                        .as_ref()
                        .map_or(0, |w| w.dropped.load(Ordering::Relaxed)),
                );
                match self.session.exited() {
                    Ok(Some(code)) => {
                        self.error = Some(format!(
                            "CPU exited {code}. Logs: {}",
                            self.session.directory.display()
                        ))
                    }
                    Err(e) => self.error = Some(e.to_string()),
                    _ => {}
                }
                if self
                    .session
                    .ipc
                    .read_rgba(&mut self.sequence, &mut self.rgba)
                {
                    self.frame =
                        image::Handle::from_rgba(WIDTH as u32, HEIGHT as u32, self.rgba.clone());
                }
            }
            _ => {}
        }
        self.sync_keys();
        Task::none()
    }
    fn view(&self, id: window::Id) -> Element<'_, Message> {
        if self.terminal.window == Some(id) {
            return self.terminal.view();
        }
        let stats = self.session.ipc.stats();
        let lost = stats.dropped_frames
            + self
                .worker
                .as_ref()
                .map_or(0, |w| w.dropped.load(Ordering::Relaxed));
        let underruns = self
            .audio
            .as_ref()
            .map_or(0, |a| a.underruns.load(Ordering::Relaxed));
        let status = if let Some(error) = &self.error {
            error.clone()
        } else {
            format!(
                "{} · {:.1} с{}{}",
                if self.paused {
                    "Пауза"
                } else {
                    "Працює"
                },
                stats.ticks as f64 / 240_000_000.0,
                if self.audio.is_none() {
                    " · аудіопристрій недоступний"
                } else {
                    ""
                },
                if lost + underruns > 0 {
                    format!(" · пропуски звуку: {}", lost + underruns)
                } else {
                    String::new()
                }
            )
        };
        let mut keys = row![].spacing(4);
        for (i, (name, _)) in BUTTONS.iter().enumerate() {
            keys = keys.push(
                button(text(*name).size(13))
                    .padding([5, 8])
                    .on_press(Message::Tap(i)),
            );
        }
        let mut layout = column![
            container(
                image(self.frame.clone())
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .content_fit(iced::ContentFit::Contain)
                    .filter_method(image::FilterMethod::Nearest)
            )
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .height(Length::Fill)
            .style(|_| container::Style::default().background(Color::BLACK)),
            container(
                column![
                    keys,
                    row![
                        text(status).size(12),
                        iced::widget::space::horizontal(),
                        text("P · пауза     F5 · перезапуск").size(12)
                    ]
                ]
                .spacing(8)
            )
            .padding([10, 12])
        ]
        .height(Length::Fill);
        if self.terminal.mode != lilka_emulator::preferences::TerminalMode::Window {
            layout = layout.push(self.terminal.view());
        }
        layout.into()
    }
    fn toolbar(&self) -> iced::widget::Row<'_, Message> {
        row![
            button(if self.paused {
                "Продовжити"
            } else {
                "Пауза"
            })
            .on_press(Message::Pause),
            button("Перезапуск").on_press(Message::Reset),
            button("UART").on_press(Message::Terminal(terminal::Message::Open))
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center)
    }
    fn standalone_view(&self, window: window::Id) -> Element<'_, Message> {
        if self.terminal.window == Some(window) {
            return self.terminal.view();
        }
        column![
            container(self.toolbar()).padding([10, 12]),
            self.view(window)
        ]
        .height(Length::Fill)
        .into()
    }
}
impl Drop for App {
    fn drop(&mut self) {
        let stats = self.session.ipc.stats();
        let device_dropped = self
            .worker
            .as_ref()
            .map_or(0, |w| w.dropped.load(Ordering::Relaxed));
        let audio_gaps = self
            .audio
            .as_ref()
            .map_or(0, |a| a.underruns.load(Ordering::Relaxed));
        self.worker.take();
        if let Err(error) = self.session.stop() {
            self.error = Some(format!("Stopping CPU: {error:#}"));
        }
        let result = serde_json::json!({"virtual_seconds":stats.ticks as f64/240_000_000.0,
            "ipc_dropped_frames":stats.dropped_frames,"device_dropped_frames":device_dropped,
            "audio_gaps":audio_gaps,"audio_device":self.audio.is_some(),"error":self.error,
            "lcd_pixels":stats.lcd_pixels,"pcm_frames":stats.pcm_frames});
        if let Ok(bytes) = serde_json::to_vec_pretty(&result) {
            let _ = std::fs::write(self.session.directory.join("desktop-result.json"), bytes);
        }
    }
}
fn host_cpu_level() -> &'static str {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::__cpuid_count;
        let (basic, leaf1, leaf7, extended, ext1) = (
            __cpuid_count(0, 0),
            __cpuid_count(1, 0),
            __cpuid_count(7, 0),
            __cpuid_count(0x8000_0000, 0),
            __cpuid_count(0x8000_0001, 0),
        );
        let required1 = (1 << 0)
            | (1 << 9)
            | (1 << 12)
            | (1 << 13)
            | (1 << 19)
            | (1 << 20)
            | (1 << 22)
            | (1 << 23)
            | (1 << 26)
            | (1 << 28)
            | (1 << 29);
        let required7 = (1 << 3) | (1 << 5) | (1 << 8);
        if basic.eax >= 7
            && extended.eax >= 0x8000_0001
            && leaf1.ecx & required1 == required1
            && leaf7.ebx & required7 == required7
            && ext1.ecx & ((1 << 0) | (1 << 5)) == ((1 << 0) | (1 << 5))
            && std::is_x86_feature_detected!("avx")
        {
            return "x86-64-v3";
        }
    }
    "portable"
}

pub fn replace_process(profile: Option<&str>) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let mut command = std::process::Command::new(std::env::current_exe()?);
    if let Some(profile) = profile {
        command.args(["--profile", profile]);
    }
    Err(command.exec().into())
}

pub fn run() -> Result<()> {
    #[cfg(target_os = "linux")]
    if let Some(directory) = std::env::current_exe()?.parent() {
        let config = directory.join("fonts.conf");
        if config.is_file() {
            // SAFETY: The entry point runs before any desktop, audio or CPU threads.
            unsafe { std::env::set_var("FONTCONFIG_FILE", config) };
        }
    }
    let mut args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "--headless") {
        return crate::headless::run();
    }
    let profile_id = if args.first().is_some_and(|a| a == "--profile") {
        ensure!(args.len() == 2, "usage: lilka-emulator --profile ID");
        let id = args[1].to_str().context("Invalid profile ID")?.to_owned();
        args.clear();
        Some(id)
    } else {
        None
    };
    if args.is_empty() {
        let executable = std::env::current_exe()?;
        let directory = executable.parent().unwrap();
        let bundle = std::iter::once(directory.to_path_buf())
            .chain(
                directory
                    .ancestors()
                    .take(4)
                    .map(|root| root.join("dist/lilka-emulator")),
            )
            .find(|path| path.join("lilka-emulator").is_file())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Не знайдено QEMU backend. Запустіть Lilka з каталогу встановленої програми."
                )
            })?;
        args = vec![
            bundle.join("lilka-emulator").into_os_string(),
            bundle
                .join("liblilka_qemu_board_bridge.so")
                .into_os_string(),
            bundle.join("rom").into_os_string(),
            lilka_emulator::preferences::user_dir().into_os_string(),
        ];
    }
    if args.len() == 1 && args[0] == "--host-cpu-level" {
        println!("{}", host_cpu_level());
        return Ok(());
    }
    if args.len() == 4 {
        let session = if let Some(id) = profile_id {
            let mut profiles = lilka_emulator::profiles::Profiles::load()?;
            let profile = profiles
                .profiles
                .iter_mut()
                .find(|p| p.id == id)
                .context("Profile no longer exists")?;
            let config = Config {
                binary: args[0].clone().into(),
                bridge: args[1].clone().into(),
                rom_dir: args[2].clone().into(),
                firmware: profile.settings.firmware.clone().into(),
                sd: profile.settings.sd.path.clone().into(),
                state_dir: profile.directory().join("state"),
                wifi_ssid: profile
                    .settings
                    .wifi
                    .internet
                    .then(|| profile.settings.wifi.ssid.clone()),
            };
            let saved = (!profile.settings.installed_state.is_empty())
                .then(|| PathBuf::from(&profile.settings.installed_state));
            let session = Session::start_desktop(config, None, saved.as_deref())?;
            profile.settings.installed_state = session.directory.display().to_string();
            profiles.save()?;
            Some(session)
        } else {
            None
        };
        return launcher::run(session);
    }
    ensure!(
        args.len() == 6 || (args.len() == 8 && (args[6] == "--app" || args[6] == "--resume")),
        "usage: lilka-qemu QEMU BRIDGE ROM_DIR STATE_DIR (profiles), or QEMU BRIDGE ROM_DIR BASE_FLASH SD_FOLDER STATE_DIR [--app APP.bin | --resume SESSION_DIR]"
    );
    let config = Config {
        binary: PathBuf::from(&args[0]),
        bridge: PathBuf::from(&args[1]),
        rom_dir: PathBuf::from(&args[2]),
        firmware: PathBuf::from(&args[3]),
        sd: PathBuf::from(&args[4]),
        state_dir: PathBuf::from(&args[5]),
        wifi_ssid: std::env::var("LILKA_QEMU_WIFI_SSID").ok(),
    };
    let app = (args.len() == 8 && args[6] == "--app").then(|| PathBuf::from(&args[7]));
    let resume = (args.len() == 8 && args[6] == "--resume").then(|| PathBuf::from(&args[7]));
    let session = Session::start_desktop(config, app.as_deref(), resume.as_deref())?;
    eprintln!("Session storage: {}", session.directory.display());
    let initial = std::cell::RefCell::new(Some(session));
    iced::daemon(
        move || {
            let app = App::new(initial.borrow_mut().take().expect("one desktop session"));
            let (_, open) = window::open(window_settings());
            (app, open.map(|_| Message::Tick))
        },
        App::update,
        App::standalone_view,
    )
    .title(|app: &App, id| {
        if app.terminal.window == Some(id) {
            "Lilka · UART".to_owned()
        } else {
            "Lilka".to_owned()
        }
    })
    .theme(theme())
    .default_font(iced::Font::with_name("Fira Sans"))
    .subscription(App::subscription)
    .run()?;
    Ok(())
}

fn window_settings() -> window::Settings {
    window::Settings {
        size: Size::new(900.0, 880.0),
        min_size: Some(Size::new(680.0, 520.0)),
        exit_on_close_request: false,
        ..Default::default()
    }
}
fn theme() -> Theme {
    Theme::custom(
        "Lilka",
        iced::theme::Palette {
            background: Color::from_rgb8(36, 36, 36),
            text: Color::from_rgb8(224, 224, 224),
            primary: Color::from_rgb8(92, 92, 92),
            success: Color::from_rgb8(144, 144, 144),
            warning: Color::from_rgb8(176, 176, 176),
            danger: Color::from_rgb8(192, 192, 192),
        },
    )
}
