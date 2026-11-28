use iced::{
    Element, Length, Task,
    widget::{
        button, column, container, pick_list, row, scrollable, text, text_editor, text_input,
    },
    window,
};
use lilka_emulator::{
    preferences::{Settings, TerminalMode},
    qemu_live::Session,
};
use std::{
    collections::VecDeque,
    fs::File,
    io::{Read, Seek},
    path::Path,
};

#[derive(Clone, Debug)]
pub(super) enum Message {
    Input(String),
    Send,
    Edit(text_editor::Action),
    Script,
    Editor,
    Load,
    Clear,
    Copy,
    Control(u8),
    Mode(TerminalMode),
    Open,
}
pub(super) struct Terminal {
    pub mode: TerminalMode,
    pub window: Option<window::Id>,
    input: String,
    script: Option<text_editor::Content>,
    output: String,
    logs: [Option<File>; 2],
    utf8: [Vec<u8>; 2],
    offsets: [u64; 2],
    escape: [u8; 2],
    pending: VecDeque<u8>,
    error: String,
}
const INPUT_LIMIT: usize = 1024 * 1024;
const OUTPUT_LIMIT: usize = 60_000;
const READ_LIMIT: u64 = 65_536;

impl Terminal {
    pub fn new() -> Self {
        Self {
            mode: Settings::load().terminal,
            window: None,
            input: String::new(),
            script: None,
            output: String::new(),
            logs: [None, None],
            utf8: [Vec::new(), Vec::new()],
            offsets: [0; 2],
            escape: [0; 2],
            pending: VecDeque::new(),
            error: String::new(),
        }
    }
    fn append_text(&mut self, port: usize, text: &str) {
        for ch in text.chars() {
            match self.escape[port] {
                1 => {
                    self.escape[port] = match ch {
                        '[' => 2,
                        ']' => 3,
                        _ => 0,
                    }
                }
                2 => {
                    if ('@'..='~').contains(&ch) {
                        self.escape[port] = 0;
                    }
                }
                3 => {
                    if ch == '\x07' {
                        self.escape[port] = 0;
                    } else if ch == '\x1b' {
                        self.escape[port] = 4;
                    }
                }
                4 => self.escape[port] = if ch == '\\' { 0 } else { 3 },
                _ => match ch {
                    '\x1b' => self.escape[port] = 1,
                    '\x08' => {
                        if !self.output.ends_with('\n') {
                            self.output.pop();
                        }
                    }
                    '\r' | '\0' => {}
                    _ => self.output.push(ch),
                },
            }
        }
    }
    fn enqueue(&mut self, bytes: &[u8]) -> bool {
        if self.pending.len() + bytes.len() > INPUT_LIMIT {
            self.error = "Черга UART заповнена. Дочекайтеся завершення передачі.".into();
            false
        } else {
            self.pending.extend(bytes);
            self.error.clear();
            true
        }
    }
    pub fn tick(&mut self, session: &Session) {
        let queued = self.pending.make_contiguous();
        let sent = session.ipc.send_uart(queued);
        self.pending.drain(..sent);
        self.read_logs(&session.directory);
    }
    fn read_logs(&mut self, directory: &Path) {
        for (port, name) in ["uart.log", "usb-serial.log"].into_iter().enumerate() {
            if self.logs[port].is_none() {
                self.logs[port] = File::open(directory.join(name)).ok();
            }
            let Some(file) = &mut self.logs[port] else {
                continue;
            };
            let Ok(metadata) = file.metadata() else {
                continue;
            };
            if metadata.len() < self.offsets[port] {
                if file.rewind().is_err() {
                    continue;
                }
                self.offsets[port] = 0;
                self.utf8[port].clear();
                self.escape[port] = 0;
            }
            if metadata.len() == self.offsets[port] {
                continue;
            }
            let before = self.utf8[port].len();
            let result = file.take(READ_LIMIT).read_to_end(&mut self.utf8[port]);
            self.offsets[port] += (self.utf8[port].len() - before) as u64;
            if let Err(error) = result {
                self.error = error.to_string();
            }
            self.decode(port);
        }
    }
    fn decode(&mut self, port: usize) {
        let mut bytes = std::mem::take(&mut self.utf8[port]);
        let mut consumed = 0;
        loop {
            let remaining = &bytes[consumed..];
            let (valid, invalid) = match std::str::from_utf8(remaining) {
                Ok(_) => (remaining.len(), None),
                Err(error) => (error.valid_up_to(), error.error_len()),
            };
            self.append_text(port, std::str::from_utf8(&remaining[..valid]).unwrap());
            consumed += valid;
            if let Some(count) = invalid {
                self.output.push('\u{fffd}');
                consumed += count;
            } else {
                break;
            }
        }
        bytes.drain(..consumed);
        self.utf8[port] = bytes;
        if self.output.len() > OUTPUT_LIMIT {
            let mut start = self.output.len() - OUTPUT_LIMIT;
            while !self.output.is_char_boundary(start) {
                start += 1;
            }
            self.output.drain(..start);
        }
    }
    pub(super) fn update(&mut self, msg: Message) -> Task<super::Message> {
        match msg {
            Message::Input(input) => self.input = input,
            Message::Send => {
                let command = self.input.clone() + "\r";
                if self.enqueue(command.as_bytes()) {
                    self.input.clear();
                }
            }
            Message::Edit(action) => {
                if let Some(editor) = &mut self.script {
                    editor.perform(action);
                }
            }
            Message::Editor => {
                self.script.get_or_insert_with(text_editor::Content::new);
            }
            Message::Script => {
                if let Some(editor) = &self.script {
                    self.enqueue(editor.text().as_bytes());
                }
            }
            Message::Control(byte) => {
                self.enqueue(&[byte]);
            }
            Message::Clear => self.output.clear(),
            Message::Copy => return iced::clipboard::write(self.output.clone()),
            Message::Load => {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("Lua", &["lua"])
                    .pick_file()
                {
                    if std::fs::metadata(&path).is_ok_and(|m| m.len() > INPUT_LIMIT as u64) {
                        self.error = "Скрипт перевищує 1 MiB".into();
                        return Task::none();
                    }
                    match std::fs::read_to_string(path) {
                        Ok(s) if s.len() <= INPUT_LIMIT => {
                            self.script = Some(text_editor::Content::with_text(&s))
                        }
                        Ok(_) => self.error = "Скрипт перевищує 1 MiB".into(),
                        Err(e) => self.error = e.to_string(),
                    }
                }
            }
            Message::Mode(mode) => {
                self.mode = mode;
                let mut settings = Settings::load();
                settings.terminal = mode;
                if let Err(e) = settings.save() {
                    self.error = e.to_string();
                }
                if mode != TerminalMode::Window {
                    if let Some(id) = self.window.take() {
                        return window::close(id);
                    }
                } else {
                    return self.update(Message::Open);
                }
            }
            Message::Open => {
                if self.mode == TerminalMode::Window {
                    if self.window.is_none() {
                        let (id, task) = window::open(window::Settings {
                            size: iced::Size::new(850.0, 650.0),
                            exit_on_close_request: false,
                            ..Default::default()
                        });
                        self.window = Some(id);
                        return task.map(|_| super::Message::Tick);
                    }
                    if let Some(id) = self.window {
                        return window::gain_focus(id);
                    }
                } else {
                    return iced::widget::operation::focus("uart-command");
                }
            }
        }
        Task::none()
    }
    pub(super) fn view(&self) -> Element<'_, super::Message> {
        let mut body = column![
            row![
                text("UART"),
                pick_list(TerminalMode::ALL, Some(self.mode), Message::Mode),
                button("Відкрити").on_press(Message::Open),
                button("Очистити").on_press(Message::Clear),
                button("Копіювати").on_press(Message::Copy),
            ]
            .spacing(8),
        ]
        .spacing(8);
        body = body
            .push(
                scrollable(text(&self.output).font(iced::Font::MONOSPACE).size(12))
                    .height(if self.mode == TerminalMode::Window {
                        Length::Fill
                    } else {
                        Length::Fixed(180.0)
                    })
                    .anchor_bottom(),
            )
            .push(
                row![
                    text_input("Lua REPL — команда", &self.input)
                        .id("uart-command")
                        .on_input(Message::Input)
                        .on_submit(Message::Send),
                    button("Надіслати ↵").on_press(Message::Send)
                ]
                .spacing(8),
            );
        if let Some(editor) = &self.script {
            body = body.push(
                text_editor(editor)
                    .placeholder("Lua Live — вставте скрипт")
                    .height(110)
                    .on_action(Message::Edit),
            );
        }
        body = body.push(
            row![
                button("Lua Live").on_press(Message::Editor),
                button("Відкрити .lua").on_press(Message::Load),
                button("Надіслати скрипт")
                    .on_press_maybe(self.script.as_ref().map(|_| Message::Script)),
                button("Ctrl-C").on_press(Message::Control(3)),
                button("Ctrl-D").on_press(Message::Control(4))
            ]
            .spacing(8),
        );
        if !self.error.is_empty() {
            body = body.push(text(&self.error));
        }
        if !self.pending.is_empty() {
            body = body.push(text(format!("У черзі: {} байтів", self.pending.len())));
        }
        container(Element::from(body).map(super::Message::Terminal))
            .padding(10)
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn output_handles_split_ansi_and_keeps_unicode() {
        let mut terminal = Terminal::new();
        terminal.append_text(0, "\x1b[1;");
        terminal.append_text(0, "32mПривіт\x1b[0m\r\nabc\x08d");
        assert_eq!(terminal.output, "Привіт\nabd");
    }
    #[test]
    fn decoding_preserves_partial_utf8_and_replaces_invalid_bytes() {
        let mut terminal = Terminal::new();
        terminal.utf8[0].extend_from_slice(&[0xd0]);
        terminal.decode(0);
        assert!(terminal.output.is_empty());
        terminal.utf8[0].extend_from_slice(&[0x9f, 0xff, b'!']);
        terminal.decode(0);
        assert_eq!(terminal.output, "П�!");
        assert!(terminal.utf8[0].is_empty());
    }
    #[test]
    fn output_limit_preserves_character_boundaries() {
        let mut terminal = Terminal::new();
        terminal.utf8[0] = "я".repeat(OUTPUT_LIMIT).into_bytes();
        terminal.decode(0);
        assert_eq!(terminal.output, "я".repeat(OUTPUT_LIMIT / 2));
    }
    #[test]
    fn logs_resume_after_idle_and_truncation() -> std::io::Result<()> {
        let directory = std::env::temp_dir().join(format!("lilka-terminal-{}", std::process::id()));
        std::fs::create_dir_all(&directory)?;
        let path = directory.join("uart.log");
        std::fs::write(&path, b"boot\x1b[")?;
        let mut terminal = Terminal::new();
        terminal.read_logs(&directory);
        assert_eq!(terminal.output, "boot");
        terminal.read_logs(&directory);
        assert_eq!(terminal.output, "boot");
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new().append(true).open(&path)?;
            file.write_all(b"32m ok")?;
        }
        terminal.read_logs(&directory);
        assert_eq!(terminal.output, "boot ok");
        std::fs::write(&path, "новий")?;
        terminal.read_logs(&directory);
        assert_eq!(terminal.output, "boot okновий");
        drop(terminal);
        std::fs::remove_dir_all(directory)
    }
    #[test]
    fn full_queue_keeps_unsent_command() {
        let mut terminal = Terminal::new();
        terminal.pending.resize(INPUT_LIMIT, 0);
        terminal.input = "print(42)".into();
        let _ = terminal.update(Message::Send);
        assert_eq!(terminal.input, "print(42)");
        assert_eq!(terminal.pending.len(), INPUT_LIMIT);
        assert!(!terminal.error.is_empty());
    }
}
