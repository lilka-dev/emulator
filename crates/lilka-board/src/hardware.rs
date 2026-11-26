use anyhow::{Result, ensure};
use std::collections::{BTreeMap, VecDeque};

pub trait Bus {
    fn read(&mut self, address: u32) -> Result<u32>;
    fn write(&mut self, address: u32, value: u32) -> Result<()>;
    fn irq(&mut self, source: u32, level: bool) -> Result<()>;
    fn bytes(&mut self, address: u32, length: usize) -> Result<Vec<u8>> {
        let mut out = vec![0; length];
        self.read_into(address, &mut out)?;
        Ok(out)
    }
    fn read_into(&mut self, address: u32, out: &mut [u8]) -> Result<()> {
        ram(address, out.len())?;
        for (i, chunk) in out.chunks_mut(4).enumerate() {
            let word = self.read(address + i as u32 * 4)?.to_le_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
        Ok(())
    }
    fn put(&mut self, address: u32, bytes: &[u8]) -> Result<()> {
        ram(address, bytes.len())?;
        for (i, chunk) in bytes.chunks(4).enumerate() {
            let a = address + i as u32 * 4;
            let mut word = if chunk.len() == 4 {
                [0; 4]
            } else {
                self.read(a)?.to_le_bytes()
            };
            word[..chunk.len()].copy_from_slice(chunk);
            self.write(a, u32::from_le_bytes(word))?;
        }
        Ok(())
    }
}
pub(crate) fn ram(a: u32, n: usize) -> Result<()> {
    let end = a.checked_add(n as u32).unwrap_or(0);
    ensure!(
        end >= a
            && ((a >= 0x3fc88000 && end <= 0x3fd00000) || (a >= 0x3c000000 && end <= 0x3e000000)),
        "DMA outside data RAM: {a:08x}+{n}"
    );
    Ok(())
}
#[path = "hardware/audio.rs"]
mod audio;
pub use audio::wav;

const DMA: u32 = 0x6003f000;
pub const AUDIO_RATE: u32 = 48000;
type AudioDescriptor = (usize, u32, u32, u32, u32);

#[derive(serde::Serialize)]
pub struct DmaTrace {
    kind: &'static str,
    source_frame: u64,
    channel: usize,
    descriptor: u32,
    raw: u32,
    value: u32,
    hash: u64,
}

struct Registers {
    banks: [Box<[u32]>; 5],
    other: BTreeMap<u32, u32>,
}
impl Default for Registers {
    fn default() -> Self {
        Self {
            banks: [0x400, 0x100, 0x100, 0x100, 0x100].map(|n| vec![0; n / 4].into_boxed_slice()),
            other: BTreeMap::new(),
        }
    }
}
impl Registers {
    fn slot(address: u32) -> Option<(usize, usize)> {
        if address & 3 != 0 {
            return None;
        }
        for (bank, base, length) in [
            (0, DMA, 0x400),
            (1, 0x6000f000, 0x100),
            (2, 0x6002d000, 0x100),
            (3, 0x60019000, 0x100),
            (4, 0x60004554, 0x100),
        ] {
            if let Some(offset) = address.checked_sub(base).filter(|n| *n < length) {
                return Some((bank, offset as usize / 4));
            }
        }
        None
    }
    fn get(&self, address: &u32) -> Option<&u32> {
        if let Some((bank, offset)) = Self::slot(*address) {
            Some(&self.banks[bank][offset])
        } else {
            self.other.get(address)
        }
    }
    fn insert(&mut self, address: u32, value: u32) {
        if let Some((bank, offset)) = Self::slot(address) {
            self.banks[bank][offset] = value;
        } else {
            self.other.insert(address, value);
        }
    }
}

#[derive(Default)]
pub struct Hardware {
    regs: Registers,
    cursor: [[u32; 2]; 5],
    running: [[bool; 2]; 5],
    audio_offset: [usize; 2],
    audio_desc: [Option<AudioDescriptor>; 2],
    audio_phase: [audio::Phase; 2],
    audio_formats: Option<[Option<audio::Format>; 2]>,
    audio_chunks: [Vec<u8>; 2],
    audio_chunk_offset: [usize; 2],
    audio_fetch_limit: [usize; 2],
    held: [[i16; 2]; 2],
    buzzer_phase: f64,
    sample_fraction: f64,
    pub samples: VecDeque<[i16; 2]>,
    pub i2s_frames: u64,
    pub buzzer_frames: u64,
    pub dropped_frames: u64,
    pub dma_descriptors: u64,
    pub dma_trace: Option<Vec<DmaTrace>>,
    audio_hash: [u64; 2],
    pub audio_completion_queue: bool,
    pub audio_completion_overflows: u64,
    audio_completions: [VecDeque<u32>; 5],
    /// Play only I2S audio the firmware wrote, never stale ring buffers; see
    /// `Hardware::audio_paused`. Off by default; the browser build enables it.
    pub audio_wait_refill: bool,
    pub audio_refill_waits: u64,
    pub audio_refill_skips: u64,
    audio_played: std::collections::HashMap<u32, u64>,
    audio_rings: [Vec<(u32, usize)>; 2],
    audio_looping: [bool; 2],
    audio_waiting: [Option<audio::RefillWait>; 2],
    audio_scratch: Vec<u8>,
}
impl Hardware {
    pub fn reset_peripherals(&mut self, mask0: u32, mask1: u32) {
        if mask1 & (1 << 6) != 0 {
            self.regs.banks[0].fill(0);
            self.cursor = [[0; 2]; 5];
            self.running = [[false; 2]; 5];
            self.audio_desc = [None; 2];
            self.audio_offset = [0; 2];
            self.audio_chunk_offset = [0; 2];
            self.audio_fetch_limit = [0; 2];
            for chunk in &mut self.audio_chunks {
                chunk.clear();
            }
            for pending in &mut self.audio_completions {
                pending.clear();
            }
            self.audio_played.clear();
            self.audio_waiting = [None; 2];
            self.audio_rings = Default::default();
            self.audio_looping = [false; 2];
        }
        for (i, bit) in [4, 21].into_iter().enumerate() {
            if mask0 & (1 << bit) != 0 {
                self.regs.banks[1 + i].fill(0);
                self.audio_phase[i] = Default::default();
                self.held[i] = [0; 2];
                self.audio_waiting[i] = None;
            }
        }
        if mask0 & (1 << 11) != 0 {
            self.regs.banks[3].fill(0);
            self.buzzer_phase = 0.0;
        }
        self.audio_formats = None;
    }
    fn reg(&self, a: u32) -> u32 {
        self.regs.get(&a).copied().unwrap_or(0)
    }
    pub fn handles(a: u32) -> bool {
        (DMA..DMA + 0x400).contains(&a)
    }
    pub fn read(&self, a: u32) -> u32 {
        match a {
            0x6000_6174 => return self.reg(a) | (1 << 16),
            0x6000_e040..=0x6000_e05c => return (self.reg(a) & 0x3fff_ffff) | (7 << 24),
            0x6000_e000..=0x6000_efff => return self.reg(a) & !(1 << 25),
            _ => {}
        }
        if !Self::handles(a) {
            return self.reg(a);
        }
        let off = (a - DMA) % 0xc0;
        if a < DMA + 0x3c0 && (off == 0xc || off == 0x6c) {
            return self.reg(a - 4) & self.reg(a + 4);
        }
        if off == 0x20 || off == 0x80 {
            let ch = ((a - DMA) / 0xc0) as usize;
            let d = usize::from(off == 0x80);
            if ch < 5 {
                return self.reg(a)
                    | if self.running[ch][d] {
                        0
                    } else {
                        1 << if d == 0 { 24 } else { 23 }
                    };
            }
        }
        self.reg(a)
    }
    pub fn write(&mut self, b: &mut impl Bus, a: u32, v: u32) -> Result<()> {
        self.audio_formats = None;
        if !Self::handles(a) {
            let v = if matches!(a, 0x6000f020 | 0x6000f024 | 0x6002d020 | 0x6002d024) {
                v & !(1 << 8)
            } else {
                v
            };
            self.regs.insert(a, v);
            return Ok(());
        }
        self.audio_desc = [None; 2];
        let ch = ((a - DMA) / 0xc0) as usize;
        let off = (a - DMA) % 0xc0;
        if ch >= 5 {
            self.regs.insert(a, v);
            return Ok(());
        }
        let d = usize::from(off >= 0x60);
        let local = off - d as u32 * 0x60;
        let base = DMA + ch as u32 * 0xc0 + d as u32 * 0x60;
        match local {
            8 | 12 => {}
            0x14 => {
                self.trace_dma("ack", ch, d, self.reg(base + 0x28), v, 0);
                let mut raw = self.reg(base + 8) & !v;
                if d == 1
                    && v & 2 != 0
                    && self.audio_completion_queue
                    && matches!(self.reg(base + 0x48) & 63, 3 | 4)
                {
                    self.audio_completions[ch].pop_front();
                    if let Some(&next) = self.audio_completions[ch].front() {
                        self.regs.insert(base + 0x28, next);
                        raw |= 2;
                    }
                }
                self.regs.insert(base + 8, raw);
            }
            0x20 => {
                let start = 1 << if d == 0 { 22 } else { 21 };
                let restart = 1 << if d == 0 { 23 } else { 22 };
                let stop = 1 << if d == 0 { 21 } else { 20 };
                if v & (start | restart) != 0 {
                    if v & start != 0 || self.cursor[ch][d] == 0 {
                        self.cursor[ch][d] = 0x3fc00000 | (v & 0xfffff);
                    }
                    self.running[ch][d] = true;
                }
                if v & stop != 0 {
                    self.running[ch][d] = false;
                }
                self.regs.insert(a, v & !(start | restart | stop));
            }
            0 => {
                self.regs.insert(a, v & !1);
                if v & 1 != 0 {
                    self.running[ch][d] = false;
                    self.cursor[ch][d] = 0;
                    self.regs.insert(base + 8, 0);
                    if d == 1 {
                        self.audio_completions[ch].clear();
                    }
                }
            }
            _ => {
                self.regs.insert(a, v);
            }
        }
        self.update_irq(b, ch, d)
    }
    fn update_irq(&self, b: &mut impl Bus, ch: usize, d: usize) -> Result<()> {
        let base = DMA + ch as u32 * 0xc0 + d as u32 * 0x60;
        b.irq(
            66 + ch as u32 + d as u32 * 5,
            self.reg(base + 8) & self.reg(base + 0x10) != 0,
        )
    }
    fn channel(&self, peripheral: u32, d: usize) -> Option<usize> {
        (0..5).find(|&ch| {
            self.running[ch][d]
                && self.reg(DMA + ch as u32 * 0xc0 + d as u32 * 0x60 + 0x48) & 63 == peripheral
        })
    }
    fn desc(&self, b: &mut impl Bus, ch: usize, d: usize) -> Result<(u32, u32, u32, u32)> {
        let a = self.cursor[ch][d];
        ram(a, 12)?;
        Ok((a, b.read(a)?, b.read(a + 4)?, b.read(a + 8)?))
    }
    fn done(
        &mut self,
        b: &mut impl Bus,
        ch: usize,
        d: usize,
        descriptor: (u32, u32, u32),
        n: usize,
        eof: bool,
    ) -> Result<()> {
        let (a, flags, next) = descriptor;
        let base = DMA + ch as u32 * 0xc0 + d as u32 * 0x60;
        if eof && self.dma_trace.is_some() {
            let peripheral = self.reg(base + 0x48) & 63;
            let hash = peripheral
                .checked_sub(3)
                .and_then(|i| self.audio_hash.get(i as usize))
                .copied()
                .unwrap_or(0);
            self.trace_dma("eof", ch, d, a, n as u32, hash);
        }
        let flags = if d == 0 {
            (flags & !0xc0fff000) | ((n as u32) << 12) | if eof { 1 << 30 } else { 0 }
        } else {
            if self.reg(base) & 4 != 0 {
                flags & !0x80000000
            } else {
                flags
            }
        };
        b.write(a, flags)?;
        self.dma_descriptors += 1;
        self.cursor[ch][d] = next;
        self.regs.insert(base + 0x30, a);
        let mut bits = 1;
        if eof {
            bits |= 2;
            let address = if self.audio_completion_queue
                && d == 1
                && matches!(self.reg(base + 0x48) & 63, 3 | 4)
            {
                let pending = &mut self.audio_completions[ch];
                if pending.len() < 64 {
                    pending.push_back(a);
                } else {
                    self.audio_completion_overflows += 1;
                }
                *pending.front().unwrap()
            } else {
                a
            };
            self.regs.insert(base + 0x28, address);
        }
        if next == 0 {
            self.running[ch][d] = false;
            if d == 1 {
                bits |= 8
            }
        }
        self.regs.insert(base + 8, self.reg(base + 8) | bits);
        self.update_irq(b, ch, d)
    }
    fn trace_dma(
        &mut self,
        kind: &'static str,
        ch: usize,
        d: usize,
        descriptor: u32,
        value: u32,
        hash: u64,
    ) {
        if self.dma_trace.is_none() || d != 1 {
            return;
        }
        let base = DMA + ch as u32 * 0xc0 + 0x60;
        if !matches!(self.reg(base + 0x48) & 63, 3 | 4) {
            return;
        }
        let raw = self.reg(base + 8);
        if let Some(trace) = &mut self.dma_trace {
            if trace.len() < 65536 {
                trace.push(DmaTrace {
                    kind,
                    source_frame: self.i2s_frames,
                    channel: ch,
                    descriptor,
                    raw,
                    value,
                    hash,
                });
            }
        }
    }
    fn error(&mut self, b: &mut impl Bus, ch: usize, d: usize) -> Result<()> {
        let base = DMA + ch as u32 * 0xc0 + d as u32 * 0x60;
        self.running[ch][d] = false;
        self.regs
            .insert(base + 8, self.reg(base + 8) | if d == 0 { 8 } else { 4 });
        self.update_irq(b, ch, d)
    }
    pub fn spi_tx(&mut self, b: &mut impl Bus, peripheral: u32, n: usize) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(n);
        self.spi_tx_into(b, peripheral, n, &mut out)?;
        Ok(out)
    }
    pub fn spi_tx_into(
        &mut self,
        b: &mut impl Bus,
        peripheral: u32,
        n: usize,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        out.clear();
        let ch = self.channel(peripheral, 1).ok_or_else(|| {
            anyhow::anyhow!("SPI TX has no started GDMA channel for peripheral {peripheral}")
        })?;
        for _ in 0..256 {
            let (a, f, buffer, next) = self.desc(b, ch, 1)?;
            let length = ((f >> 12) & 4095) as usize;
            if (f >> 31 == 0 && self.reg(DMA + ch as u32 * 0xc0 + 0x64) & (1 << 12) != 0)
                || length == 0
                || length > (f & 4095) as usize
            {
                self.error(b, ch, 1)?;
                break;
            }
            let used = length.min(n - out.len());
            let start = out.len();
            out.resize(start + used, 0);
            b.read_into(buffer, &mut out[start..])?;
            self.done(b, ch, 1, (a, f, next), used, f & (1 << 30) != 0)?;
            if out.len() == n {
                break;
            }
            if next == 0 || f & (1 << 30) != 0 {
                break;
            }
        }
        ensure!(
            out.len() == n,
            "GDMA TX chain shorter than SPI transfer: {} / {n}",
            out.len()
        );
        Ok(())
    }
    pub fn spi_rx(&mut self, b: &mut impl Bus, peripheral: u32, bytes: &[u8]) -> Result<()> {
        let ch = self.channel(peripheral, 0).ok_or_else(|| {
            anyhow::anyhow!("SPI RX has no started GDMA channel for peripheral {peripheral}")
        })?;
        let mut offset = 0;
        for _ in 0..256 {
            let (a, f, buffer, next) = self.desc(b, ch, 0)?;
            let size = (f & 4095) as usize;
            if (f >> 31 == 0 && self.reg(DMA + ch as u32 * 0xc0 + 4) & (1 << 12) != 0) || size == 0
            {
                self.error(b, ch, 0)?;
                break;
            }
            let n = size.min(bytes.len() - offset);
            b.put(buffer, &bytes[offset..offset + n])?;
            offset += n;
            self.done(b, ch, 0, (a, f, next), n, offset == bytes.len())?;
            if offset == bytes.len() || next == 0 {
                break;
            }
        }
        ensure!(offset == bytes.len(), "GDMA RX chain too short");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Memory {
        words: BTreeMap<u32, u32>,
        irqs: BTreeMap<u32, bool>,
    }
    impl Bus for Memory {
        fn read(&mut self, a: u32) -> Result<u32> {
            let mut bytes = [0; 4];
            for (i, byte) in bytes.iter_mut().enumerate() {
                let address = a + i as u32;
                *byte = (self.words.get(&(address & !3)).copied().unwrap_or(0)
                    >> ((address & 3) * 8)) as u8;
            }
            Ok(u32::from_le_bytes(bytes))
        }
        fn write(&mut self, a: u32, v: u32) -> Result<()> {
            for (i, byte) in v.to_le_bytes().into_iter().enumerate() {
                let address = a + i as u32;
                let word = self.words.entry(address & !3).or_default();
                *word = (*word & !(255 << ((address & 3) * 8)))
                    | ((byte as u32) << ((address & 3) * 8));
            }
            Ok(())
        }
        fn irq(&mut self, s: u32, v: bool) -> Result<()> {
            self.irqs.insert(s, v);
            Ok(())
        }
    }
    fn descriptor(b: &mut Memory, a: u32, flags: u32, buffer: u32, next: u32) {
        b.write(a, flags).unwrap();
        b.write(a + 4, buffer).unwrap();
        b.write(a + 8, next).unwrap();
    }
    #[test]
    fn analog_calibration_is_ready_without_a_radio() {
        let mut hardware = Hardware::default();
        let mut memory = Memory::default();
        let control = 0x6000_e040;
        assert_ne!(hardware.read(control) & (1 << 24), 0);
        hardware.write(&mut memory, control, 8).unwrap();
        assert_eq!(hardware.read(control) & 0xff, 8);
        assert_ne!(hardware.read(control) & (1 << 24), 0);
        hardware.write(&mut memory, 0x6000_e000, 1 << 25).unwrap();
        assert_eq!(hardware.read(0x6000_e000) & (1 << 25), 0);
        assert_ne!(hardware.read(0x6000_6174) & (1 << 16), 0);
    }
    #[test]
    fn system_dma_reset_clears_inflight_work_but_keeps_i2s_and_capture() {
        let mut h = Hardware::default();
        h.audio_completion_queue = true;
        h.regs.insert(0x6000f030, 0x12345678);
        h.regs.insert(DMA + 0x68, 3);
        h.cursor[0][1] = 0x3fc90000;
        h.running[0][1] = true;
        h.audio_completions[0].push_back(0x3fc90000);
        h.audio_desc[0] = Some((0, 0x3fc90000, 128 << 12, 0x3fca0000, 0));
        h.audio_offset[0] = 16;
        h.audio_chunks[0] = vec![1; 128];
        h.audio_chunk_offset[0] = 16;
        h.samples.push_back([123; 2]);
        h.i2s_frames = 17;
        h.reset_peripherals(0, 1 << 6);
        assert_eq!(h.read(DMA + 0x68), 0);
        assert!(!h.running[0][1]);
        assert_eq!(h.cursor[0][1], 0);
        assert!(h.audio_completions[0].is_empty());
        assert!(h.audio_desc[0].is_none());
        assert!(h.audio_chunks[0].is_empty());
        assert_eq!(h.audio_offset[0], 0);
        assert_eq!(h.audio_chunk_offset[0], 0);
        assert_eq!(h.read(0x6000f030), 0x12345678);
        assert_eq!(h.samples.front(), Some(&[123; 2]));
        assert_eq!(h.i2s_frames, 17);
        assert!(h.audio_completion_queue);
    }
    #[test]
    fn chained_rx_sets_lengths_ownership_eof_and_irq_clear() -> Result<()> {
        for ch in 0..5 {
            let mut h = Hardware::default();
            let mut b = Memory::default();
            let base = DMA + ch * 0xc0;
            descriptor(&mut b, 0x3fc90000, 0x80000003, 0x3fca0001, 0x3fc9000c);
            descriptor(&mut b, 0x3fc9000c, 0x80000005, 0x3fca0004, 0);
            h.write(&mut b, base + 0x48, 1)?;
            h.write(&mut b, base + 0x10, 3)?;
            h.write(&mut b, base + 0x20, 0x90000 | (1 << 22))?;
            h.spi_rx(&mut b, 1, b"abcde")?;
            assert_eq!(b.bytes(0x3fca0001, 5)?, b"abcde");
            assert_eq!(b.read(0x3fc90000)?, 3 | (3 << 12));
            assert_eq!(b.read(0x3fc9000c)?, 5 | (2 << 12) | (1 << 30));
            assert_eq!(h.read(base + 0xc), 3);
            assert_eq!(b.irqs.get(&(66 + ch)), Some(&true));
            h.write(&mut b, base + 0x14, 3)?;
            assert_eq!(h.read(base + 0xc), 0);
            assert_eq!(b.irqs.get(&(66 + ch)), Some(&false));
        }
        Ok(())
    }
    #[test]
    fn tx_writeback_is_optional_and_ownership_error_is_reported() -> Result<()> {
        let mut h = Hardware::default();
        let mut b = Memory::default();
        let base = DMA + 0x60;
        descriptor(&mut b, 0x3fc90000, 0xc0001001, 0x3fca0000, 0);
        b.put(0x3fca0000, b"x")?;
        h.write(&mut b, base + 0x20, 0x90000 | (1 << 21))?;
        assert_eq!(h.spi_tx(&mut b, 0, 1)?, b"x");
        assert_eq!(b.read(0x3fc90000)?, 0xc0001001);
        descriptor(&mut b, 0x3fc90000, 0x40001001, 0x3fca0000, 0);
        h.write(&mut b, base + 0x14, !0)?;
        h.write(&mut b, base + 4, 1 << 12)?;
        h.write(&mut b, base + 0x10, 4)?;
        h.write(&mut b, base + 0x20, 0x90000 | (1 << 21))?;
        assert!(h.spi_tx(&mut b, 0, 1).is_err());
        assert_eq!(h.read(base + 8), 4);
        assert_eq!(b.irqs.get(&71), Some(&true));
        Ok(())
    }
    #[test]
    fn cyclic_i2s_dma_and_pin_routing() -> Result<()> {
        let mut h = Hardware::default();
        let mut b = Memory::default();
        for (a, v) in [
            (0x600045fc, 28),
            (0x60004558, 29),
            (0x6000455c, 30),
            (0x6002d024, 4),
            (0x6002d02c, (31 << 24) | (31 << 13) | (9 << 7)),
            (0x6002d034, (2 << 27) | 10),
            (0x6002d054, 1),
            (DMA + 0x60, 0),
            (DMA + 0xa8, 4),
        ] {
            h.write(&mut b, a, v)?;
        }
        descriptor(&mut b, 0x3fc90000, 0xc0004004, 0x3fca0000, 0x3fc90000);
        b.write(0x3fca0000, 0x12340000)?;
        h.write(&mut b, DMA + 0x80, 0x90000 | (1 << 21))?;
        h.tick(&mut b, 1000.0)?;
        assert!(h.i2s_frames >= 49);
        assert!(h.drain_audio().iter().all(|f| *f == [0x1234; 2]));
        h.write(&mut b, 0x6000455c, 256)?;
        h.tick(&mut b, 1000.0)?;
        assert!(h.drain_audio().iter().all(|f| *f == [0; 2]));
        Ok(())
    }
    #[test]
    fn audio_event_peek_predicts_eof_without_consuming_samples() -> Result<()> {
        let mut h = Hardware::default();
        let mut b = Memory::default();
        for (a, v) in [
            (0x600045fc, 28),
            (0x60004558, 29),
            (0x6000455c, 30),
            (0x6002d024, 4),
            (0x6002d02c, (31 << 24) | (31 << 13) | (9 << 7)),
            (0x6002d034, (2 << 27) | 10),
            (0x6002d054, 1),
            (DMA + 0xa8, 4),
        ] {
            h.write(&mut b, a, v)?;
        }
        descriptor(&mut b, 0x3fc90000, 0xc0200200, 0x3fca0000, 0x3fc90000);
        h.write(&mut b, DMA + 0x80, 0x90000 | (1 << 21))?;
        assert_eq!(h.next_audio_frames(), Some(1));
        let words = b.words.clone();
        h.prepare_audio_events(&mut b);
        assert_eq!(h.next_audio_frames(), Some(123));
        assert_eq!(h.i2s_frames, 0);
        assert_eq!(h.dma_descriptors, 0);
        assert!(h.samples.is_empty());
        assert_eq!(b.words, words);
        assert_eq!(h.read(DMA + 0x68), 0);
        Ok(())
    }
    #[test]
    fn queued_i2s_completions_survive_delayed_acknowledgement_and_reset() -> Result<()> {
        let mut h = Hardware {
            audio_completion_queue: true,
            ..Hardware::default()
        };
        let mut b = Memory::default();
        let base = DMA + 0x60;
        h.write(&mut b, base + 0x48, 3)?;
        h.write(&mut b, base + 0x10, 2)?;
        let addresses = [0x3fc90000, 0x3fc9000c, 0x3fc90018];
        for &a in &addresses {
            h.done(&mut b, 0, 1, (a, 0xc0004004, a + 12), 4, true)?;
        }
        assert_eq!(h.read(base + 0x28), addresses[0]);
        h.write(&mut b, base + 0x14, 1)?;
        assert_eq!(h.read(base + 0x28), addresses[0]);
        for (i, &a) in addresses.iter().enumerate() {
            assert_eq!(h.read(base + 0x28), a);
            assert_eq!(b.irqs.get(&71), Some(&true));
            h.write(&mut b, base + 0x14, 2)?;
            assert_eq!(h.read(base + 0xc), if i == 2 { 0 } else { 2 });
        }
        assert_eq!(b.irqs.get(&71), Some(&false));
        for _ in 0..66 {
            h.done(
                &mut b,
                0,
                1,
                (addresses[0], 0xc0004004, addresses[0]),
                4,
                true,
            )?;
        }
        assert_eq!(h.audio_completions[0].len(), 64);
        assert_eq!(h.audio_completion_overflows, 2);
        h.write(&mut b, base, 1)?;
        assert!(h.audio_completions[0].is_empty());
        assert_eq!(b.irqs.get(&71), Some(&false));
        Ok(())
    }
}
