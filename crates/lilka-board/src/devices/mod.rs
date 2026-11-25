pub mod display;
pub mod sd;

use crate::bridge::{Device, Pins, Transaction};
use anyhow::{Result, ensure};
use display::Display;
use sd::SdCard;

pub struct LilkaDevices {
    pub display: Display,
    pub sd: SdCard,
    pub display_transfers: usize,
    pub sd_transfers: usize,
    pub strict_shared_selects: bool,
    pub shared_select_transfers: usize,
}

impl Default for LilkaDevices {
    fn default() -> Self {
        Self {
            display: Display::default(),
            sd: SdCard::demo(),
            display_transfers: 0,
            sd_transfers: 0,
            strict_shared_selects: false,
            shared_select_transfers: 0,
        }
    }
}

impl LilkaDevices {
    fn check_selects(&mut self, transaction: &Transaction) -> Result<()> {
        if transaction.display_cs && transaction.sd_cs {
            self.shared_select_transfers += 1;
            ensure!(
                !self.strict_shared_selects,
                "SPI bus collision: display and SD selected together"
            );
        }
        Ok(())
    }
    pub fn exchange_write(&mut self, transaction: &Transaction) -> Result<()> {
        self.check_selects(transaction)?;
        let display_result = if transaction.display_cs {
            self.display_transfers += 1;
            self.display.write(transaction.dc, &transaction.mosi)
        } else {
            Ok(())
        };
        if transaction.sd_cs {
            self.sd_transfers += 1;
            for byte in &transaction.mosi {
                self.sd.clock(*byte);
            }
        }
        display_result
    }
}
impl Device for LilkaDevices {
    fn pins(&mut self, pins: Pins) {
        self.sd.select(pins.low & (1 << 16) == 0);
        self.display.power(pins.high & (1 << (46 - 32)) != 0);
    }
    fn exchange(&mut self, transaction: &Transaction) -> Result<Vec<u8>> {
        self.check_selects(transaction)?;
        let display_result = if transaction.display_cs {
            self.display_transfers += 1;
            self.display.write(transaction.dc, &transaction.mosi)
        } else {
            Ok(())
        };
        let miso = if transaction.sd_cs {
            self.sd_transfers += 1;
            transaction
                .mosi
                .iter()
                .map(|byte| self.sd.clock(*byte))
                .collect()
        } else {
            vec![0xff; transaction.mosi.len()]
        };
        display_result?;
        Ok(miso)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_simultaneous_selects_reject_before_devices_change() {
        let mut devices = LilkaDevices::default();
        devices.strict_shared_selects = true;
        let transaction = Transaction {
            pc: 0,
            mosi: vec![0],
            miso: Vec::new(),
            read: true,
            display_cs: true,
            sd_cs: true,
            dc: false,
        };
        assert!(devices.exchange(&transaction).is_err());
        assert_eq!(devices.display_transfers, 0);
        assert_eq!(devices.sd_transfers, 0);
    }
    #[test]
    fn simultaneous_selects_broadcast_pixels_without_miso_contention() -> Result<()> {
        for read in [false, true] {
            let mut devices = LilkaDevices::default();
            devices.display.power(true);
            devices.display.write(false, &[0x11, 0x29, 0x2c])?;
            devices.sd.select(true);
            let transaction = Transaction {
                pc: 0,
                mosi: vec![0x12, 0x34],
                miso: vec![],
                read,
                display_cs: true,
                sd_cs: true,
                dc: true,
            };
            if read {
                assert_eq!(devices.exchange(&transaction)?, vec![0xff; 2]);
            } else {
                devices.exchange_write(&transaction)?;
            }
            assert_eq!(devices.display.pixel(0, 0), 0x1234);
            assert_eq!(
                (
                    devices.display_transfers,
                    devices.sd_transfers,
                    devices.shared_select_transfers
                ),
                (1, 1, 1)
            );
        }
        Ok(())
    }
    #[test]
    fn simultaneous_selects_return_sd_cmd0_response() -> Result<()> {
        let mut devices = LilkaDevices::default();
        devices.display.power(true);
        devices.sd.select(true);
        let transaction = Transaction {
            pc: 0,
            mosi: vec![0x40, 0, 0, 0, 0, 0x95, 0xff, 0xff],
            miso: vec![],
            read: true,
            display_cs: true,
            sd_cs: true,
            dc: false,
        };
        let miso = devices.exchange(&transaction)?;
        assert!(miso[6..].contains(&1), "{miso:?}");
        assert_eq!(devices.display_transfers, 1);
        assert_eq!(devices.sd_transfers, 1);
        Ok(())
    }
}
