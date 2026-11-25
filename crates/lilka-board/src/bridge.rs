use anyhow::Result;
use serde::Serialize;
#[derive(Clone, Copy, Debug, Default)]
pub struct Pins {
    pub low: u32,
    pub high: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct Transaction {
    pub pc: u32,
    pub mosi: Vec<u8>,
    pub read: bool,
    pub display_cs: bool,
    pub sd_cs: bool,
    pub dc: bool,
    pub miso: Vec<u8>,
}

pub trait Device {
    fn pins(&mut self, _pins: Pins) {}
    fn exchange(&mut self, transaction: &Transaction) -> Result<Vec<u8>>;
}
