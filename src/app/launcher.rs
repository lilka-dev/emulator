use super::App;
use anyhow::{Context, Result, ensure};
use iced::{
    Element, Event, Length, Subscription, Task, keyboard,
    widget::{button, checkbox, column, container, row, scrollable, text, text_input},
    window,
};
use lilka_emulator::{
    controls::{BUTTONS, key_name},
    preferences::{Settings, user_dir},
    profiles::Profiles,
    storage::{SdMode, atomic_write},
};
use std::path::PathBuf;

#[derive(Clone, Debug)]
enum Message {
    Runtime(super::Message),
    Name(String),
    Select(String),
    Create,
    Firmware,
    Rename,
    OpenFolder,
    Folder,
    OwnFolder,
    Wifi(bool),
    Ssid(String),
    Start,
    Home,
    Settings,
    Bind(usize),
    Defaults,
    Event(Event),
    Close,
}

struct Desktop {
    profiles: Profiles,
    name: String,
    error: String,
    running: Option<App>,
    settings: bool,
    binding: Option<usize>,
    pause_before_settings: bool,
    keys: [String; 10],
}

impl Desktop {
    fn save_keys(&mut self) -> Result<()> {
        atomic_write(
            &user_dir().join("controls.json"),
            &serde_json::to_vec_pretty(&self.keys)?,
        )?;
        if let Some(app) = &mut self.running {
            app.keys = self.keys.clone();
            app.held.fill(false);
            app.sync_keys();
        }
        Ok(())
    }

    fn firmware(&mut self) -> Result<()> {
        let Some(source) = rfd::FileDialog::new()
            .set_title("Прошивка Lilka")
            .add_filter("Прошивка", &["bin"])
            .pick_file()
        else {
            return Ok(());
        };
        ensure!(
            std::fs::metadata(&source)?.len() <= 16 * 1024 * 1024,
            "Прошивка перевищує 16 MiB"
        );
        let bytes = std::fs::read(&source)?;
        let base = if bytes.get(0x8000..0x8002) == Some(&[0xaa, 0x50]) {
            None
        } else {
            let Some(base) = rfd::FileDialog::new()
                .set_title("Базова прошивка з bootloader")
                .add_filter("Прошивка", &["bin"])
                .pick_file()
            else {
                return Ok(());
            };
            Some(base)
        };
        let id = self.profiles.active.clone().context("Створіть профіль")?;
        self.profiles
            .profiles
            .iter_mut()
            .find(|p| p.id == id)
            .unwrap()
            .install_firmware(&source, base.as_deref())?;
        self.profiles.save()
    }

    fn change(&mut self, message: Message) -> Result<Task<Message>> {
        match message {
            Message::Close => return Ok(iced::exit()),
            Message::Name(value) => self.name = value,
            Message::Select(name) => {
                self.profiles.select(&name)?;
                self.profiles.save()?;
            }
            Message::Create => {
                self.profiles.create(&self.name, Settings::default())?;
                self.profiles.save()?;
                self.name.clear();
                self.firmware()?;
            }
            Message::Firmware => self.firmware()?,
            Message::Wifi(_) | Message::Ssid(_) => {
                let id = self.profiles.active.clone().context("Оберіть профіль")?;
                let profile = self
                    .profiles
                    .profiles
                    .iter_mut()
                    .find(|p| p.id == id)
                    .unwrap();
                match message {
                    Message::Wifi(enabled) => profile.settings.wifi.internet = enabled,
                    Message::Ssid(ssid) => profile.settings.wifi.ssid = ssid,
                    _ => unreachable!(),
                }
                self.profiles.save()?;
            }
            Message::Rename => {
                let name = self.name.trim();
                ensure!(!name.is_empty(), "Введіть нову назву");
                let id = self.profiles.active.clone().context("Оберіть профіль")?;
                ensure!(
                    !self
                        .profiles
                        .profiles
                        .iter()
                        .any(|p| p.id != id && p.name.eq_ignore_ascii_case(name)),
                    "Така назва вже використовується"
                );
                self.profiles
                    .profiles
                    .iter_mut()
                    .find(|p| p.id == id)
                    .unwrap()
                    .name = name.into();
                self.profiles.save()?;
                self.name.clear();
            }
            Message::OpenFolder => {
                let profile = self.profiles.active().context("Оберіть профіль")?;
                #[cfg(target_os = "linux")]
                let command = "xdg-open";
                #[cfg(target_os = "macos")]
                let command = "open";
                #[cfg(target_os = "windows")]
                let command = "explorer";
                std::process::Command::new(command)
                    .arg(&profile.settings.sd.path)
                    .spawn()?;
            }
            Message::Folder | Message::OwnFolder => {
                let own = matches!(message, Message::OwnFolder);
                let selected = if own {
                    self.profiles.active().map(|p| p.directory().join("sd"))
                } else {
                    rfd::FileDialog::new()
                        .set_title("Тека SD-карти")
                        .pick_folder()
                };
                if let Some(path) = selected {
                    std::fs::create_dir_all(&path)?;
                    let id = self.profiles.active.clone().context("Створіть профіль")?;
                    let profile = self
                        .profiles
                        .profiles
                        .iter_mut()
                        .find(|p| p.id == id)
                        .unwrap();
                    profile.settings.sd.mode = SdMode::Folder;
                    profile.settings.sd.path = path.display().to_string();
                    profile.settings.installed_state.clear();
                    self.profiles.save()?;
                }
            }
            Message::Start => {
                let profile = self.profiles.active().context("Оберіть профіль")?;
                ensure!(!profile.settings.firmware.is_empty(), "Оберіть прошивку");
                ensure!(
                    profile.settings.sd.mode == SdMode::Folder,
                    "Оберіть теку SD для цього профілю"
                );
                let id = self.profiles.active.clone().context("Оберіть профіль")?;
                self.profiles.save()?;
                super::replace_process(Some(&id))?;
            }
            Message::Home => {
                if let Some(app) = &mut self.running {
                    app.session.stop()?;
                }
                self.running.take();
                super::replace_process(None)?;
                self.settings = false;
                self.binding = None;
            }
            Message::Settings => {
                self.settings = !self.settings;
                self.binding = None;
                if let Some(app) = &mut self.running {
                    app.held.fill(false);
                    app.pulse.fill(None);
                    app.sync_keys();
                    if self.settings {
                        self.pause_before_settings = app.paused;
                    }
                    app.paused = self.settings || self.pause_before_settings;
                    app.session.pause(app.paused);
                }
            }
            Message::Bind(index) => self.binding = Some(index),
            Message::Defaults => {
                self.keys = Settings::default().keys;
                self.save_keys()?;
            }
            Message::Runtime(super::Message::Close) => return Ok(iced::exit()),
            Message::Runtime(super::Message::Event(event)) | Message::Event(event)
                if self.settings =>
            {
                if let Event::Keyboard(keyboard::Event::KeyPressed { key, .. }) = event
                    && let Some(index) = self.binding
                {
                    let name = key_name(&key);
                    ensure!(
                        !name.is_empty() && name != "p" && name != "F5",
                        "Ця клавіша зарезервована"
                    );
                    if let Some(other) = self.keys.iter().position(|value| value == &name) {
                        self.keys.swap(index, other);
                    } else {
                        self.keys[index] = name;
                    }
                    self.binding = None;
                    self.save_keys()?;
                }
            }
            Message::Runtime(message) => {
                if let Some(app) = &mut self.running {
                    return Ok(app.update(message).map(Message::Runtime));
                }
            }
            Message::Event(Event::Keyboard(keyboard::Event::KeyPressed { key, .. }))
                if self.running.is_none() && !self.settings && key_name(&key) == "Enter" =>
            {
                return self.change(Message::Start);
            }
            _ => {}
        }
        Ok(Task::none())
    }
    fn update(&mut self, message: Message) -> Task<Message> {
        if !matches!(message, Message::Runtime(super::Message::Tick)) {
            self.error.clear();
        }
        match self.change(message) {
            Ok(task) => task,
            Err(error) => {
                self.error = format!("{error:#}");
                Task::none()
            }
        }
    }
    fn subscription(&self) -> Subscription<Message> {
        if let Some(app) = &self.running {
            app.subscription().map(Message::Runtime)
        } else {
            Subscription::batch([
                iced::event::listen().map(Message::Event),
                window::close_requests().map(|_| Message::Close),
            ])
        }
    }
    fn view(&self, window: window::Id) -> Element<'_, Message> {
        let mut toolbar = row![
            text("Lilka").size(20),
            button("Профілі").on_press(Message::Home),
            button(if self.settings {
                "Назад"
            } else {
                "Налаштування"
            })
            .on_press(Message::Settings)
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center);
        if let Some(app) = &self.running {
            toolbar = toolbar.push(iced::widget::space::horizontal());
            if !self.settings {
                toolbar = toolbar.push(Element::from(app.toolbar()).map(Message::Runtime));
            }
        }
        let content: Element<'_, Message> = if self.settings {
            let mut controls = column![
                text("Керування").size(23),
                text("Натисніть кнопку, а потім потрібну клавішу.").width(Length::Fill),
                text("P — пауза, F5 — перезапуск.")
            ]
            .spacing(12)
            .width(Length::Fill);
            for (i, (name, _)) in BUTTONS.iter().enumerate() {
                controls = controls.push(
                    row![
                        text(*name).width(120),
                        button(text(if self.binding == Some(i) {
                            "Натисніть клавішу…"
                        } else {
                            &self.keys[i]
                        }))
                        .on_press(Message::Bind(i))
                    ]
                    .spacing(12),
                );
            }
            scrollable(controls.push(button("Типове керування").on_press(Message::Defaults))).into()
        } else if let Some(app) = &self.running {
            app.view(window).map(Message::Runtime)
        } else {
            let mut profiles = column![text("Профілі").size(24)].spacing(10);
            for profile in &self.profiles.profiles {
                let title = if Some(&profile.id) == self.profiles.active.as_ref() {
                    format!("{} · обрано", profile.name)
                } else {
                    profile.name.clone()
                };
                profiles = profiles
                    .push(button(text(title)).on_press(Message::Select(profile.name.clone())));
            }
            profiles = profiles.push(
                row![
                    text_input("Назва профілю", &self.name).on_input(Message::Name),
                    button("Створити").on_press(Message::Create),
                    button("Перейменувати").on_press(Message::Rename)
                ]
                .spacing(12),
            );
            if let Some(profile) = self.profiles.active() {
                profiles = profiles
                    .push(text(&profile.name).size(22))
                    .push(text(if profile.settings.firmware.is_empty() {
                        "Прошивка ще не встановлена".to_owned()
                    } else if profile.firmware_name.is_empty() {
                        "Прошивка встановлена".to_owned()
                    } else {
                        format!("Прошивка: {}", profile.firmware_name)
                    }))
                    .push(button("Прошити…").on_press(Message::Firmware))
                    .push(text(
                        if PathBuf::from(&profile.settings.sd.path)
                            == profile.directory().join("sd")
                        {
                            "SD: тека профілю".to_owned()
                        } else {
                            format!(
                                "SD: {}",
                                PathBuf::from(&profile.settings.sd.path)
                                    .file_name()
                                    .unwrap_or_default()
                                    .to_string_lossy()
                            )
                        },
                    ))
                    .push(
                        row![
                            button("Обрати теку…").on_press(Message::Folder),
                            button("Тека профілю").on_press(Message::OwnFolder),
                            button("Відкрити теку").on_press(Message::OpenFolder)
                        ]
                        .spacing(12),
                    )
                    .push(
                        checkbox(profile.settings.wifi.internet)
                            .label("Wi-Fi · експериментально")
                            .on_toggle(Message::Wifi),
                    );
                if profile.settings.wifi.internet {
                    profiles = profiles
                        .push(
                            text_input(
                                "Назва відкритої точки доступу",
                                &profile.settings.wifi.ssid,
                            )
                            .on_input(Message::Ssid),
                        )
                        .push(text(
                            "Відкрита мережа без пароля. Підтримка залежить від прошивки.",
                        ));
                }
                profiles = profiles.push(button("Запустити · Enter").on_press(Message::Start));
            }
            scrollable(profiles).into()
        };
        let content = if self.running.is_some() && !self.settings {
            content
        } else {
            container(content)
                .padding(20)
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
        };
        let mut layout =
            column![container(toolbar).padding([10, 12]), content].height(Length::Fill);
        if !self.error.is_empty() {
            layout = layout.push(container(text(&self.error)).padding([8, 12]));
        }
        layout.into()
    }
}

pub fn run(session: Option<super::Session>) -> Result<()> {
    let profiles = Profiles::load()?;
    let keys = match std::fs::read(user_dir().join("controls.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("Пошкоджені налаштування керування")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Settings::default().keys,
        Err(error) => return Err(error.into()),
    };
    let initial = std::cell::RefCell::new(Some((profiles, keys, session)));
    iced::daemon(
        move || {
            let (profiles, keys, session) =
                initial.borrow_mut().take().expect("one desktop window");
            let running = session.map(|session| {
                let mut app = App::new(session);
                app.keys = keys.clone();
                app
            });
            let (_, open) = window::open(super::window_settings());
            (
                Desktop {
                    profiles,
                    name: String::new(),
                    error: String::new(),
                    running,
                    settings: false,
                    binding: None,
                    pause_before_settings: false,
                    keys,
                },
                open.map(|_| {
                    Message::Event(Event::Window(window::Event::RedrawRequested(
                        std::time::Instant::now(),
                    )))
                }),
            )
        },
        Desktop::update,
        Desktop::view,
    )
    .title(|_: &Desktop, _| "Lilka".to_owned())
    .theme(super::theme())
    .default_font(iced::Font::with_name("Fira Sans"))
    .subscription(Desktop::subscription)
    .run()?;
    Ok(())
}
