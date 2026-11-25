use crate::bus::Device;
#[derive(Default)]
pub struct Gpio {
    pub external: u64,
    pub output: u64,
    pub enable: u64,
    pub strap: u16,
}
impl Gpio {
    fn levels(&self) -> u64 {
        (self.external & !self.enable) | (self.output & self.enable)
    }
    fn update(bank: &mut u64, upper: bool, operation: u8, value: u32) {
        let shift = if upper { 32 } else { 0 };
        let value = u64::from(value) & if upper { 0x3fffff } else { u64::from(u32::MAX) };
        let mask = (if upper {
            0x3fffffu64
        } else {
            u64::from(u32::MAX)
        }) << shift;
        match operation {
            0 => *bank = (*bank & !mask) | (value << shift),
            1 => *bank |= value << shift,
            2 => *bank &= !(value << shift),
            _ => unreachable!(),
        }
    }
}
impl Device for Gpio {
    fn read(&mut self, offset: u32, width: u8, _now: u64) -> Option<u32> {
        if width != 4 {
            return None;
        }
        Some(match offset {
            4 | 8 | 0xc => self.output as u32,
            0x10 | 0x14 | 0x18 => (self.output >> 32) as u32,
            0x20 | 0x24 | 0x28 => self.enable as u32,
            0x2c | 0x30 | 0x34 => (self.enable >> 32) as u32,
            0x38 => u32::from(self.strap),
            0x3c => self.levels() as u32,
            0x40 => (self.levels() >> 32) as u32,
            _ => return None,
        })
    }
    fn write(&mut self, offset: u32, width: u8, value: u32, _now: u64) -> bool {
        if width != 4 {
            return false;
        }
        match offset {
            4 | 8 | 0xc => Self::update(&mut self.output, false, ((offset - 4) / 4) as u8, value),
            0x10 | 0x14 | 0x18 => {
                Self::update(&mut self.output, true, ((offset - 0x10) / 4) as u8, value)
            }
            0x20 | 0x24 | 0x28 => {
                Self::update(&mut self.enable, false, ((offset - 0x20) / 4) as u8, value)
            }
            0x2c | 0x30 | 0x34 => {
                Self::update(&mut self.enable, true, ((offset - 0x2c) / 4) as u8, value)
            }
            0x38 | 0x3c | 0x40 => {}
            _ => return false,
        }
        true
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn set_clear_registers_and_output_enable_preserve_other_pins() {
        let mut gpio = Gpio {
            external: 1 << 2,
            strap: 8,
            ..Default::default()
        };
        gpio.write(8, 4, 1 << 5, 0);
        gpio.write(0x24, 4, 1 << 5, 0);
        assert_eq!(gpio.read(0x3c, 4, 0), Some((1 << 2) | (1 << 5)));
        gpio.write(0xc, 4, 1 << 5, 0);
        assert_eq!(gpio.read(0x3c, 4, 0), Some(1 << 2));
        gpio.write(0x14, 4, 1 << 10, 0);
        gpio.write(0x30, 4, 1 << 10, 0);
        assert_eq!(gpio.read(0x40, 4, 0), Some(1 << 10));
        assert_eq!(gpio.read(0x38, 4, 0), Some(8));
    }
}
