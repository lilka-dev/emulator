use crate::bus::Device;
pub struct Sensors {
    registers: [u32; 22],
    pub input_uv: [[u32; 10]; 2],
    pub temperature_raw: u8,
}
impl Default for Sensors {
    fn default() -> Self {
        let mut registers = [0; 22];
        registers[0] = 0x20040002;
        registers[0x24 / 4] = 0x40050002;
        registers[0x14 / 4] = u32::MAX;
        registers[0x38 / 4] = u32::MAX;
        registers[0x50 / 4] = (6 << 14) | (1 << 12);
        registers[0x54 / 4] = 1 << 14;
        Self {
            registers,
            input_uv: [[0; 10]; 2],
            temperature_raw: 0x80,
        }
    }
}
impl Device for Sensors {
    fn read(&mut self, offset: u32, width: u8, _now: u64) -> Option<u32> {
        if width != 4 || offset & 3 != 0 {
            return None;
        }
        if offset == 0x50 {
            let control = self.registers[0x50 / 4];
            let ready = u32::from(control & (1 << 24) != 0) << 8;
            return Some((control & !0x1ff) | ready | u32::from(self.temperature_raw));
        }
        self.registers.get((offset / 4) as usize).copied()
    }
    fn write(&mut self, offset: u32, width: u8, value: u32, _now: u64) -> bool {
        if width != 4 || offset & 3 != 0 || offset > 0x54 {
            return false;
        }
        if matches!(offset, 0xc | 0x30) {
            let unit = usize::from(offset == 0x30);
            let previous = self.registers[offset as usize / 4];
            self.registers[offset as usize / 4] = (value & 0xfffe0000) | (previous & 0x1ffff);
            if value & (1 << 17) != 0 {
                let pads = (value >> 19) & 0x3ff;
                let channel = pads.trailing_zeros() as usize;
                let sample = if channel < 10 {
                    let attenuation = (self.registers[if unit == 0 { 0x14 / 4 } else { 0x38 / 4 }]
                        >> (channel * 2))
                        & 3;
                    ((u64::from(self.input_uv[unit][channel]) * 4095)
                        / [1_100_000, 1_500_000, 2_200_000, 3_100_000][attenuation as usize])
                        .min(4095) as u32
                } else {
                    0
                };
                self.registers[offset as usize / 4] = (value & 0xfffe0000) | (1 << 16) | sample;
            }
        } else if !matches!(offset, 4 | 0x28) {
            self.registers[offset as usize / 4] = value;
        }
        true
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn temperature_conversion_publishes_ready_and_raw_data() {
        let mut sensor = Sensors::default();
        assert_eq!(sensor.read(0x50, 4, 0).unwrap() & (1 << 8), 0);
        sensor.temperature_raw = 137;
        sensor.write(0x50, 4, (1 << 24) | (1 << 22) | 0x1ff, 0);
        assert_eq!(sensor.read(0x50, 4, 0).unwrap() & 0x1ff, (1 << 8) | 137);
        sensor.write(0x50, 4, 1 << 22, 0);
        assert_eq!(sensor.read(0x50, 4, 0).unwrap() & (1 << 8), 0);
    }
    #[test]
    fn adc_pad_selection_attenuation_and_done_flag() {
        let mut adc = Sensors::default();
        adc.input_uv[0][2] = 1_000_000;
        adc.write(0xc, 4, (1 << 17) | (1 << 21), 0);
        assert_eq!(adc.read(0xc, 4, 0).unwrap() & 0x1ffff, (1 << 16) | 1320);
        adc.write(0x14, 4, 0, 0);
        adc.write(0xc, 4, (1 << 17) | (1 << 21), 0);
        assert_eq!(adc.read(0xc, 4, 0).unwrap() & 0xffff, 3722);
        adc.input_uv[0][2] = u32::MAX;
        adc.write(0xc, 4, (1 << 17) | (1 << 21), 0);
        assert_eq!(adc.read(0xc, 4, 0).unwrap() & 0xffff, 4095);
    }
}
