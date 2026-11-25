use crate::{
    bridge::{Device as SpiDevice, Pins, Transaction},
    devices::LilkaDevices,
    hardware::Hardware,
};
use crate::{
    bus::{Access, Bus, Device},
    gpio::Gpio,
};
use anyhow::{Result, bail};

pub struct BoardState {
    pub devices: LilkaDevices,
    pub gpio: Gpio,
    pub hardware: Hardware,
    pub interrupts: u128,
    pub transfers: usize,
    pub last_audio_tick: u64,
    audio_remainder: u64,
    pub errors: Vec<String>,
    pub trace_enabled: bool,
    pub trace: std::collections::VecDeque<Transaction>,
    pub sd_trace: std::collections::VecDeque<Transaction>,
}
impl Default for BoardState {
    fn default() -> Self {
        let mut devices = LilkaDevices::default();
        devices.display = crate::devices::display::Display::lilka();
        Self {
            devices,
            gpio: Gpio {
                external: ((1u64 << 49) - 1),
                strap: 8,
                ..Default::default()
            },
            hardware: Hardware::default(),
            interrupts: 0,
            transfers: 0,
            last_audio_tick: 0,
            audio_remainder: 0,
            errors: Vec::new(),
            trace_enabled: false,
            trace: Default::default(),
            sd_trace: Default::default(),
        }
    }
}

struct HardwareBus<'a> {
    memory: &'a mut dyn Bus,
    interrupts: &'a mut u128,
}
impl crate::hardware::Bus for HardwareBus<'_> {
    fn read_into(&mut self, address: u32, out: &mut [u8]) -> Result<()> {
        crate::hardware::ram(address, out.len())?;
        Ok(self.memory.read_bytes(address, out)?)
    }
    fn put(&mut self, address: u32, bytes: &[u8]) -> Result<()> {
        crate::hardware::ram(address, bytes.len())?;
        Ok(self.memory.write_bytes(address, bytes)?)
    }
    fn read(&mut self, address: u32) -> Result<u32> {
        Ok(self.memory.read(address, 4, Access::Read)?)
    }
    fn write(&mut self, address: u32, value: u32) -> Result<()> {
        Ok(self.memory.write(address, 4, value)?)
    }
    fn irq(&mut self, source: u32, level: bool) -> Result<()> {
        if source >= 128 {
            bail!("invalid interrupt source");
        }
        if level {
            *self.interrupts |= 1 << source;
        } else {
            *self.interrupts &= !(1 << source);
        }
        Ok(())
    }
}
fn push_trace(trace: &mut std::collections::VecDeque<Transaction>, transaction: Transaction) {
    if trace.len() == 256 {
        trace.pop_front();
    }
    trace.push_back(transaction);
}
struct SpiPort {
    index: u32,
    registers: [u32; 64],
    scratch: Vec<u8>,
}
impl SpiPort {
    fn transfer(&mut self, state: &mut BoardState, memory: &mut dyn Bus) {
        let BoardState {
            devices,
            gpio,
            hardware,
            interrupts,
            errors,
            trace_enabled,
            transfers,
            trace,
            sd_trace,
            ..
        } = state;
        *transfers += 1;
        let user = self.registers[0x10 / 4];
        let dma = self.registers[0x30 / 4];
        let length = if user & (3 << 27) != 0 {
            ((self.registers[0x1c / 4] & 0x3ffff) + 1).div_ceil(8) as usize
        } else {
            0
        };
        if length > 32768 {
            errors.push(format!("SPI transfer exceeds 32 KiB: {length}"));
            return;
        }
        let pins = Pins {
            low: gpio.output as u32 | (!(gpio.enable as u32) & ((1 << 7) | (1 << 16))),
            high: (gpio.output >> 32) as u32,
        };
        devices.pins(pins);
        let mut bus = HardwareBus { memory, interrupts };
        let transfer = (|| -> Result<()> {
            let mut mosi = if user & (1 << 27) != 0 && dma & (1 << 27) != 0 {
                hardware.spi_tx_into(&mut bus, self.index, length, &mut self.scratch)?;
                std::mem::take(&mut self.scratch)
            } else {
                let mut data = std::mem::take(&mut self.scratch);
                data.resize(length, 255);
                data.fill(255);
                if user & (1 << 27) != 0 {
                    let start = if user & (1 << 25) != 0 { 32 } else { 0 };
                    for (i, b) in data.iter_mut().enumerate() {
                        let n = start + i;
                        if n < 64 {
                            *b = (self.registers[0x98 / 4 + n / 4] >> (8 * (n % 4))) as u8;
                        }
                    }
                }
                data
            };
            if self.registers[2] & (1 << 25) != 0 {
                for b in &mut mosi {
                    *b = b.reverse_bits();
                }
            }
            let mut transaction = Transaction {
                pc: 0,
                mosi,
                read: user & (1 << 28) != 0,
                display_cs: pins.low & (1 << 7) == 0,
                sd_cs: pins.low & (1 << 16) == 0,
                dc: pins.low & (1 << 15) != 0,
                miso: Vec::new(),
            };
            if transaction.read {
                let mut miso = devices.exchange(&transaction)?;
                if self.registers[2] & (1 << 23) != 0 {
                    for b in &mut miso {
                        *b = b.reverse_bits();
                    }
                }
                if *trace_enabled {
                    transaction.miso = miso.clone();
                }
                if dma & (1 << 28) != 0 {
                    hardware.spi_rx(&mut bus, self.index, &miso)?;
                } else {
                    let start = if user & (1 << 24) != 0 { 32 } else { 0 };
                    for (i, b) in miso.iter().enumerate() {
                        let n = start + i;
                        if n < 64 {
                            let word = &mut self.registers[0x98 / 4 + n / 4];
                            *word = (*word & !(255 << (8 * (n % 4))))
                                | (u32::from(*b) << (8 * (n % 4)));
                        }
                    }
                }
            } else {
                devices.exchange_write(&transaction)?;
            }
            if *trace_enabled {
                if transaction.sd_cs {
                    push_trace(sd_trace, transaction.clone());
                }
                self.scratch = transaction.mosi.clone();
                push_trace(trace, transaction);
            } else {
                self.scratch = transaction.mosi;
            }
            Ok(())
        })();
        if let Err(error) = transfer {
            errors.push(error.to_string());
        }
    }
}
pub struct BoardDevice {
    pub state: BoardState,
    spi: [SpiPort; 2],
}
impl Default for BoardDevice {
    fn default() -> Self {
        Self {
            state: BoardState::default(),
            spi: std::array::from_fn(|i| SpiPort {
                index: i as u32,
                registers: [0; 64],
                scratch: Vec::new(),
            }),
        }
    }
}
impl BoardDevice {
    pub fn prepare_audio_events(&mut self, memory: &mut dyn Bus) {
        self.state.hardware.prepare_audio_events(&mut HardwareBus {
            memory,
            interrupts: &mut self.state.interrupts,
        });
    }
    pub fn reset_peripherals(&mut self, mask0: u32, mask1: u32) {
        for (port, bit) in self.spi.iter_mut().zip([6, 16]) {
            if mask0 & (1 << bit) != 0 {
                port.registers.fill(0);
                port.scratch.clear();
            }
        }
        self.state.hardware.reset_peripherals(mask0, mask1);
        if mask1 & (1 << 6) != 0 {
            self.state.interrupts &= !(((1u128 << 10) - 1) << 66);
        }
    }
    pub fn reset(&mut self, now: u64) {
        let mut next = Self::default();
        next.state.devices.strict_shared_selects = self.state.devices.strict_shared_selects;
        std::mem::swap(&mut next.state.devices.sd, &mut self.state.devices.sd);
        next.state.devices.sd.reset_bus();
        next.state.gpio.strap = self.state.gpio.strap;
        next.state.gpio.external = self.state.gpio.external;
        next.state.last_audio_tick = now;
        next.state.trace_enabled = self.state.trace_enabled;
        next.state.hardware.audio_completion_queue = self.state.hardware.audio_completion_queue;
        if let Some(trace) = &mut self.state.hardware.dma_trace {
            let mut trace = std::mem::take(trace);
            trace.clear();
            next.state.hardware.dma_trace = Some(trace);
        }
        *self = next;
    }
    pub fn spi_register_bank_mut(&mut self, index: usize) -> Option<&mut [u32; 64]> {
        self.spi.get_mut(index).map(|port| &mut port.registers)
    }
}
fn hardware_address(address: u32) -> bool {
    Hardware::handles(address)
        || (0x60019000..0x60019100).contains(&address)
        || (0x6000f000..0x6000f100).contains(&address)
        || (0x6002d000..0x6002d100).contains(&address)
        || (0x60004554..0x60004618).contains(&address)
}
impl Device for BoardDevice {
    fn next_event(&self, _now: u64) -> Option<u64> {
        self.state.hardware.next_audio_frames().map(|frames| {
            self.state.last_audio_tick.saturating_add(
                frames
                    .saturating_mul(5000)
                    .saturating_sub(self.state.audio_remainder),
            )
        })
    }
    fn read_changes_events(&self, _offset: u32) -> bool {
        false
    }
    fn write_changes_events(&self, address: u32, width: u8) -> bool {
        !(width == 4
            && address & 3 == 0
            && [0x60024000, 0x60025000]
                .into_iter()
                .any(|base| (base + 4..base + 0x100).contains(&address)))
    }
    fn interrupt_sources(&mut self, _now: u64) -> u128 {
        self.state.interrupts
    }
    fn advance(&mut self, now: u64, memory: &mut dyn Bus) {
        if let Err(error) = advance_state(&mut self.state, memory, now) {
            self.state.errors.push(error.to_string());
        }
    }
    fn read(&mut self, address: u32, width: u8, now: u64) -> Option<u32> {
        if (0x60004000..0x60004a00).contains(&address) && !hardware_address(address) {
            return self.state.gpio.read(address - 0x60004000, width, now);
        }
        if width != 4 || address & 3 != 0 {
            return None;
        }
        if hardware_address(address) {
            return Some(self.state.hardware.read(address));
        }
        for (i, base) in [0x60024000, 0x60025000].into_iter().enumerate() {
            if (base..base + 0x100).contains(&address) {
                return Some(self.spi[i].registers[(address - base) as usize / 4]);
            }
        }
        None
    }
    fn write(&mut self, _address: u32, _width: u8, _value: u32, _now: u64) -> bool {
        false
    }
    fn write_with_bus(
        &mut self,
        address: u32,
        width: u8,
        value: u32,
        now: u64,
        memory: &mut dyn Bus,
    ) -> bool {
        if hardware_address(address) {
            if width != 4 || address & 3 != 0 {
                return false;
            }
            self.advance(now, memory);
            let BoardState {
                hardware,
                interrupts,
                errors,
                ..
            } = &mut self.state;
            if let Err(error) =
                hardware.write(&mut HardwareBus { memory, interrupts }, address, value)
            {
                errors.push(error.to_string());
            }
            return true;
        }
        if (0x60004000..0x60004a00).contains(&address) {
            let handled = self
                .state
                .gpio
                .write(address - 0x60004000, width, value, now);
            if handled {
                let gpio = &self.state.gpio;
                self.state.devices.pins(Pins {
                    low: gpio.output as u32 | (!(gpio.enable as u32) & ((1 << 7) | (1 << 16))),
                    high: (gpio.output >> 32) as u32,
                });
            }
            return handled;
        }
        if width != 4 || address & 3 != 0 {
            return false;
        }
        for (i, base) in [0x60024000, 0x60025000].into_iter().enumerate() {
            if (base..base + 0x100).contains(&address) {
                let offset = (address - base) as usize / 4;
                self.spi[i].registers[offset] = value;
                if offset == 0 {
                    if value & (1 << 24) != 0 {
                        self.spi[i].transfer(&mut self.state, memory);
                    }
                    self.spi[i].registers[0] &= !((1 << 24) | (1 << 23));
                }
                return true;
            }
        }
        false
    }
}
fn advance_state(state: &mut BoardState, memory: &mut dyn Bus, now: u64) -> Result<()> {
    let elapsed = now.saturating_sub(state.last_audio_tick);
    state.last_audio_tick = now;
    let BoardState {
        hardware,
        interrupts,
        audio_remainder,
        ..
    } = state;
    let frames = elapsed / 5000 + (*audio_remainder + elapsed % 5000) / 5000;
    *audio_remainder = (*audio_remainder + elapsed % 5000) % 5000;
    let mut remaining = frames;
    while remaining != 0 {
        let count = remaining.min(4800) as usize;
        hardware.tick_frames(&mut HardwareBus { memory, interrupts }, count)?;
        remaining -= count as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selective_reset_preserves_other_domains_and_external_devices() {
        let mut board = BoardDevice::default();
        board.state.devices.sd.image[0] = 0x5a;
        let external = board.state.gpio.external;
        board.spi_register_bank_mut(0).unwrap()[1] = 0x12345678;
        board.spi_register_bank_mut(1).unwrap()[1] = 0x87654321;
        board.state.interrupts = (1 << 71) | (1 << 8);
        board.reset_peripherals(1 << 6, 0);
        assert_eq!(board.spi_register_bank_mut(0).unwrap()[1], 0);
        assert_eq!(board.spi_register_bank_mut(1).unwrap()[1], 0x87654321);
        assert_eq!(board.state.interrupts, (1 << 71) | (1 << 8));
        board.reset_peripherals(0, 1 << 6);
        assert_eq!(board.state.interrupts, 1 << 8);
        assert_eq!(board.spi_register_bank_mut(1).unwrap()[1], 0x87654321);
        assert_eq!(board.state.devices.sd.image[0], 0x5a);
        assert_eq!(board.state.gpio.external, external);
    }
    #[test]
    fn peripheral_reset_keeps_sd_and_inputs_without_replaying_old_audio_time() {
        let mut board = BoardDevice::default();
        board.state.devices.sd.image[0] = 0x5a;
        board.state.gpio.external &= !(1 << 5);
        let external = board.state.gpio.external;
        board.spi_register_bank_mut(0).unwrap()[1] = 0x12345678;
        board.state.interrupts = 1 << 71;
        board.state.hardware.audio_completion_queue = true;
        board.state.hardware.samples.push_back([123; 2]);
        board.reset(240_000_000);
        assert_eq!(board.state.devices.sd.image[0], 0x5a);
        assert_eq!(board.state.gpio.external, external);
        assert_eq!(board.spi_register_bank_mut(0).unwrap()[1], 0);
        assert_eq!(board.state.interrupts, 0);
        assert!(board.state.hardware.audio_completion_queue);
        let mut memory = crate::bus::tests::Memory::default();
        board.advance(240_005_000, &mut memory);
        assert_eq!(board.state.hardware.drain_audio(), vec![[0; 2]]);
    }
}
