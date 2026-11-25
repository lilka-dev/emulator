#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct WifiConfig {
    pub ssid: String,
    pub password: String,
    pub internet: bool,
}
impl Default for WifiConfig {
    fn default() -> Self {
        Self {
            ssid: "Lilka".into(),
            password: String::new(),
            internet: false,
        }
    }
}
