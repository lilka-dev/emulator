pub const BUTTONS: [(&str, u8); 10] = [
    ("Up", 38),
    ("Down", 41),
    ("Left", 39),
    ("Right", 40),
    ("A", 5),
    ("B", 6),
    ("C", 10),
    ("D", 9),
    ("Select", 0),
    ("Start", 4),
];
pub const DEFAULT_KEYS: [&str; 10] = [
    "ArrowUp",
    "ArrowDown",
    "ArrowLeft",
    "ArrowRight",
    "z",
    "x",
    "c",
    "v",
    "Tab",
    "Enter",
];

pub fn key_name(key: &iced::keyboard::Key) -> String {
    match key {
        iced::keyboard::Key::Character(value) => value.to_lowercase(),
        iced::keyboard::Key::Named(value) => format!("{value:?}"),
        _ => String::new(),
    }
}
