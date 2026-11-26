use super::{AUDIO_RATE, Bus, DMA, Hardware};
use anyhow::Result;

#[derive(Clone, Copy, Default)]
pub(super) struct Phase {
    remainder: u64,
    denominator: u64,
}
impl Phase {
    fn set_denominator(&mut self, denominator: u64) {
        if self.denominator != denominator {
            self.remainder = if self.denominator == 0 {
                0
            } else {
                (u128::from(self.remainder) * u128::from(denominator)
                    / u128::from(self.denominator)) as u64
            };
            self.denominator = denominator;
        }
    }
    fn advance(&mut self, numerator: u64) -> u32 {
        self.remainder += numerator;
        let mut count = 0;
        while self.remainder >= self.denominator {
            self.remainder -= self.denominator;
            count += 1;
        }
        count
    }
}
/// A channel whose ring holds no audio the firmware has written since it last played.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct RefillWait {
    /// I2S samples the channel has been paused for.
    samples: u32,
}
/// I2S samples between two rechecks of a paused ring.
const REFILL_RECHECK: u32 = 32;
/// How long a ring may stay unchanged before it counts as audio the firmware
/// means to repeat: 1 s at 44.1 kHz. It then plays as is, without pausing again,
/// until the firmware writes new audio.
const REFILL_PATIENCE: u32 = 44100;
/// What to do with the next I2S buffer under `Hardware::audio_wait_refill`.
enum Refill {
    Play,
    Skip,
    Wait,
}

/// FNV-1a of the buffer and whether it is all zeros.
fn content(scratch: &mut Vec<u8>, b: &mut impl Bus, buffer: u32, length: usize) -> Option<(u64, bool)> {
    scratch.resize(length, 0);
    b.read_into(buffer, scratch).ok()?;
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &byte in scratch.iter() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
    }
    Some((hash, scratch.iter().all(|&byte| byte == 0)))
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Format {
    numerator: u64,
    denominator: u64,
    bits: usize,
    channels: usize,
}

impl Hardware {
    pub fn prepare_audio_events(&mut self, b: &mut impl Bus) {
        for i in 0..2 {
            if self.audio_desc[i].is_some() {
                continue;
            }
            let Some(format) = self.format(i) else {
                continue;
            };
            let Some(ch) = self.channel(3 + i as u32, 1) else {
                continue;
            };
            let Ok((a, flags, buffer, next)) = self.desc(b, ch, 1) else {
                continue;
            };
            let length = ((flags >> 12) & 4095) as usize;
            let stride = (if format.bits <= 16 { 2 } else { 4 }) * format.channels;
            if length == 0
                || length > (flags & 4095) as usize
                || self.audio_offset[i] + stride > length
                || super::ram(buffer, length).is_err()
                || (flags >> 31 == 0 && self.reg(DMA + ch as u32 * 0xc0 + 0x64) & (1 << 12) != 0)
            {
                continue;
            }
            self.audio_desc[i] = Some((ch, a, flags, buffer, next));
        }
    }
    /// Whether I2S channel `i` is paused, waiting for the firmware to write audio.
    ///
    /// The DMA loops over a ring of buffers that the firmware rewrites. A CPU
    /// slower than the hardware (the browser's interpreter) falls behind, and
    /// the DMA replays stale buffers as a stretched buzz. With
    /// `audio_wait_refill` only audio the firmware wrote plays: a non-silent
    /// buffer about to replay exactly what it played last time is skipped (its
    /// EOF fires at once and returns it to the driver) while another buffer of
    /// the ring holds new audio. When none does, the channel pauses and emits no
    /// frames until one does, or until `REFILL_PATIENCE` samples have passed.
    /// Guest time keeps running: the stream carries the firmware's audio in
    /// order, only slower than real time when the CPU is.
    pub(super) fn audio_paused(&self, i: usize) -> bool {
        self.audio_waiting[i].is_some()
    }
    /// Whether a buffer of channel `i`'s ring differs from what it last played.
    fn audio_ring_has_news(&mut self, b: &mut impl Bus, i: usize) -> bool {
        for &(buffer, length) in &self.audio_rings[i] {
            if let Some((hash, silent)) = content(&mut self.audio_scratch, b, buffer, length)
                && !silent
                && self.audio_played.get(&buffer) != Some(&hash)
            {
                return true;
            }
        }
        false
    }
    fn audio_refill(&mut self, b: &mut impl Bus, i: usize, buffer: u32, length: usize) -> Refill {
        if let Some(wait) = &mut self.audio_waiting[i] {
            wait.samples += 1;
            let samples = wait.samples;
            if samples % REFILL_RECHECK != 0 {
                return Refill::Wait;
            }
            if samples >= REFILL_PATIENCE {
                self.audio_looping[i] = true;
            } else if !self.audio_ring_has_news(b, i) {
                return Refill::Wait;
            }
            self.audio_waiting[i] = None;
        }
        if !self.audio_rings[i].iter().any(|&(known, _)| known == buffer) && self.audio_rings[i].len() < 32 {
            self.audio_rings[i].push((buffer, length));
        }
        let Some((hash, silent)) = content(&mut self.audio_scratch, b, buffer, length) else {
            return Refill::Play;
        };
        if silent || self.audio_played.get(&buffer) != Some(&hash) {
            // New audio: the firmware is writing again.
            self.audio_looping[i] &= silent;
            self.audio_played.insert(buffer, hash);
            return Refill::Play;
        }
        if self.audio_looping[i] {
            return Refill::Play;
        }
        if self.audio_ring_has_news(b, i) {
            self.audio_refill_skips += 1;
            return Refill::Skip;
        }
        self.audio_waiting[i] = Some(RefillWait::default());
        self.audio_refill_waits += 1;
        Refill::Wait
    }
    pub fn audio_irq_pending(&self) -> bool {
        (0..2).any(|i| {
            self.format(i).is_some()
                && self.channel(3 + i as u32, 1).is_some_and(|ch| {
                    let base = DMA + ch as u32 * 0xc0 + 0x60;
                    self.reg(base + 8) & self.reg(base + 0x10) & 2 != 0
                })
        })
    }
    pub fn drain_audio(&mut self) -> Vec<[i16; 2]> {
        self.samples.drain(..).collect()
    }
    pub fn drain_audio_into(&mut self, output: &mut Vec<[i16; 2]>) {
        output.clear();
        output.extend(self.samples.drain(..));
    }
    pub fn tick(&mut self, b: &mut impl Bus, microseconds: f64) -> Result<()> {
        self.sample_fraction += microseconds.clamp(0.0, 100000.0) * AUDIO_RATE as f64 / 1e6;
        let frames = self.sample_fraction as usize;
        self.sample_fraction -= frames as f64;
        self.tick_frames(b, frames)
    }
    pub fn next_audio_frames(&self) -> Option<u64> {
        (0..2)
            .filter_map(|i| {
                let format = self.format(i)?;
                let _channel = self.channel(3 + i as u32, 1)?;
                let Some((_, _, flags, _, _)) = self.audio_desc[i] else {
                    return Some(1);
                };
                let stride = (if format.bits <= 16 { 2 } else { 4 }) * format.channels;
                let length = ((flags >> 12) & 4095) as usize;
                let source = length.saturating_sub(self.audio_offset[i]) / stride;
                if source == 0 {
                    return Some(1);
                }
                let denominator = u128::from(format.denominator) * u128::from(AUDIO_RATE);
                let phase = if self.audio_phase[i].denominator == denominator as u64 {
                    self.audio_phase[i].remainder
                } else {
                    0
                };
                let needed = (source as u128 * denominator).saturating_sub(u128::from(phase));
                Some(
                    needed
                        .div_ceil(u128::from(format.numerator))
                        .max(1)
                        .min(u128::from(u64::MAX)) as u64,
                )
            })
            .min()
    }
    pub fn tick_frames(&mut self, b: &mut impl Bus, frames: usize) -> Result<()> {
        if frames == 0 {
            return Ok(());
        }
        let (hz, duty) = self.buzzer();
        let formats = if let Some(formats) = self.audio_formats {
            formats
        } else {
            let formats = [self.format(0), self.format(1)];
            self.audio_formats = Some(formats);
            formats
        };
        if hz == 0.0 && formats.iter().all(Option::is_none) {
            let capacity = AUDIO_RATE as usize * 2;
            let retained = frames.min(capacity);
            let dropped = self
                .samples
                .len()
                .saturating_add(frames)
                .saturating_sub(capacity);
            self.samples.drain(..dropped.min(self.samples.len()));
            self.dropped_frames += dropped as u64;
            self.samples.extend(std::iter::repeat_n([0; 2], retained));
            return Ok(());
        }
        for (i, format) in formats.iter().enumerate() {
            self.audio_chunks[i].clear();
            self.audio_chunk_offset[i] = 0;
            self.audio_fetch_limit[i] = format.map_or(0, |f| {
                ((frames as u128 * u128::from(f.numerator)
                    / (u128::from(f.denominator) * u128::from(AUDIO_RATE)))
                    + 1)
                .min(4096) as usize
            });
        }
        for (i, format) in formats.iter().enumerate() {
            if let Some(format) = format {
                self.audio_phase[i].set_denominator(format.denominator * u64::from(AUDIO_RATE));
            } else {
                self.audio_phase[i] = Phase::default();
                self.held[i] = [0; 2];
            }
        }
        for _ in 0..frames {
            let mut mix = [0i32; 2];
            let mut paused = 0;
            if hz > 0.0 && duty > 0.0 && duty < 1.0 {
                self.buzzer_phase = (self.buzzer_phase + hz / AUDIO_RATE as f64).fract();
                let value = if self.buzzer_phase < duty {
                    3500
                } else {
                    -3500
                };
                mix = [value; 2];
                self.buzzer_frames += 1;
            }
            for (i, format) in formats.iter().enumerate() {
                if let Some(format) = *format {
                    let count = self.audio_phase[i].advance(format.numerator);
                    for _ in 0..count {
                        self.held[i] = self.pcm(b, i, format.bits, format.channels)?;
                    }
                    if self.audio_paused(i) {
                        paused += 1;
                        continue;
                    }
                    for (m, s) in mix.iter_mut().zip(self.held[i]) {
                        *m += s as i32;
                    }
                }
            }
            let active = formats.iter().flatten().count();
            if paused > 0 && paused == active && !(hz > 0.0 && duty > 0.0 && duty < 1.0) {
                // Every source waits for the firmware: no frame rather than a
                // stale or invented one (audio_paused).
                continue;
            }
            if self.samples.len() == AUDIO_RATE as usize * 2 {
                self.samples.pop_front();
                self.dropped_frames += 1;
            }
            self.samples
                .push_back(mix.map(|s| s.clamp(i16::MIN as i32, i16::MAX as i32) as i16));
        }
        Ok(())
    }
    fn buzzer(&self) -> (f64, f64) {
        let route = self.reg(0x60004580) & 511;
        if !(73..81).contains(&route) {
            return (0.0, 0.0);
        }
        let ch = route - 73;
        let conf = self.reg(0x60019000 + ch * 20);
        let timer = self.reg(0x600190a0 + (conf & 3) * 8);
        if conf & 4 == 0 || timer & (3 << 22) != 0 {
            return (0.0, 0.0);
        }
        let resolution = timer & 15;
        let divider = (timer >> 4) & 0x3ffff;
        if resolution == 0 || divider == 0 {
            return (0.0, 0.0);
        }
        let clock = if timer & (1 << 24) != 0 {
            1e6
        } else {
            match self.reg(0x600190d0) & 3 {
                1 => 80e6,
                2 => 8e6,
                3 => 40e6,
                _ => return (0.0, 0.0),
            }
        };
        let duty = self.reg(0x60019008 + ch * 20) as f64 / 16.0 / (1u32 << resolution) as f64;
        (
            clock * 256.0 / divider as f64 / (1u32 << resolution) as f64,
            duty,
        )
    }
    fn format(&self, i: usize) -> Option<Format> {
        let base = if i == 0 { 0x6000f000 } else { 0x6002d000 };
        let route = if i == 0 { [22, 24, 25] } else { [28, 29, 30] };
        if self.reg(base + 0x24) & 4 == 0
            || self.reg(0x600045fc) & 511 != route[0]
            || self.reg(0x60004558) & 511 != route[1]
            || self.reg(0x6000455c) & 511 != route[2]
        {
            return None;
        }
        let conf = self.reg(base + 0x2c);
        let clock = self.reg(base + 0x34);
        let frac = self.reg(base + 0x3c);
        let bits = (((conf >> 13) & 31) + 1) as usize;
        if ![8, 16, 24, 32].contains(&bits) {
            return None;
        }
        let channels = (((self.reg(base + 0x54) >> 16) & 15) + 1) as usize;
        if channels > 2 {
            return None;
        }
        let x = (frac >> 18) & 511;
        let y = (frac >> 9) & 511;
        let z = frac & 511;
        let (fraction, denominator) = if z == 0 {
            (0u64, 1u64)
        } else if frac & (1 << 27) != 0 {
            let denominator = u64::from((x + 1) * z + y);
            (denominator - u64::from(z), denominator)
        } else {
            let denominator = u64::from(((x + 1) * z + y).checked_sub(1)?);
            (u64::from(z), denominator)
        };
        let divider = u64::from(clock & 255) * denominator + fraction;
        let source = match (clock >> 27) & 3 {
            2 => 160_000_000u64,
            3 => 40_000_000u64,
            _ => return None,
        };
        let numerator = source * denominator;
        let denominator = divider
            * u64::from(((conf >> 7) & 63) + 1)
            * u64::from(((conf >> 24) & 31) + 1)
            * channels as u64;
        if denominator == 0 || !(4000 * denominator..=192000 * denominator).contains(&numerator) {
            return None;
        }
        Some(Format {
            numerator,
            denominator,
            bits,
            channels,
        })
    }
    fn pcm(
        &mut self,
        b: &mut impl Bus,
        i: usize,
        bits: usize,
        channels: usize,
    ) -> Result<[i16; 2]> {
        let (ch, a, f, buffer, next) = if let Some(descriptor) = self.audio_desc[i] {
            descriptor
        } else {
            let Some(ch) = self.channel(3 + i as u32, 1) else {
                return Ok([0; 2]);
            };
            let (a, f, buffer, next) = self.desc(b, ch, 1)?;
            let descriptor = (ch, a, f, buffer, next);
            self.audio_desc[i] = Some(descriptor);
            descriptor
        };
        let length = ((f >> 12) & 4095) as usize;
        let width = if bits <= 16 { 2 } else { 4 };
        let stride = width * channels;
        let offset = self.audio_offset[i];
        if offset == 0 && self.audio_wait_refill && length > 0 && length <= (f & 4095) as usize {
            match self.audio_refill(b, i, buffer, length) {
                Refill::Play => {}
                // Paused: tick_frames emits nothing for this channel.
                Refill::Wait => return Ok(self.held[i]),
                Refill::Skip => {
                    self.audio_desc[i] = None;
                    self.done(b, ch, 1, (a, f, next), length, f & (1 << 30) != 0)?;
                    return Ok(self.held[i]);
                }
            }
        }
        if offset == 0 {
            self.audio_hash[i] = 0;
        }
        if (f >> 31 == 0 && self.reg(DMA + ch as u32 * 0xc0 + 0x64) & (1 << 12) != 0)
            || length == 0
            || length > (f & 4095) as usize
            || offset + stride > length
        {
            self.error(b, ch, 1)?;
            self.audio_offset[i] = 0;
            self.audio_desc[i] = None;
            return Ok([0; 2]);
        }
        let mut bytes = [0; 8];
        if self.audio_chunk_offset[i] == self.audio_chunks[i].len() {
            let length = ((length - offset) / stride)
                .min(self.audio_fetch_limit[i].max(1))
                .min(256)
                * stride;
            self.audio_chunks[i].resize(length, 0);
            self.audio_chunk_offset[i] = 0;
            if b.read_into(buffer + offset as u32, &mut self.audio_chunks[i])
                .is_err()
            {
                self.audio_chunks[i].clear();
                b.read_into(buffer + offset as u32, &mut bytes[..stride])?;
            }
        }
        if !self.audio_chunks[i].is_empty() {
            let start = self.audio_chunk_offset[i];
            bytes[..stride].copy_from_slice(&self.audio_chunks[i][start..start + stride]);
            self.audio_chunk_offset[i] += stride;
        }
        let mut sample = [0; 2];
        for c in 0..channels {
            sample[c] = if width == 2 {
                i16::from_le_bytes([bytes[c * 2], bytes[c * 2 + 1]])
            } else {
                (i32::from_le_bytes(bytes[c * 4..c * 4 + 4].try_into().unwrap()) >> 16) as i16
            };
        }
        if channels == 1 {
            sample[1] = sample[0]
        }
        if self.dma_trace.is_some() {
            let packed = sample[0] as u16 as u64 | ((sample[1] as u16 as u64) << 16);
            self.audio_hash[i] = self.audio_hash[i].rotate_left(5) ^ packed;
        }
        self.audio_fetch_limit[i] = self.audio_fetch_limit[i].saturating_sub(1);
        self.audio_offset[i] += stride;
        self.i2s_frames += 1;
        if self.audio_offset[i] == length {
            self.audio_offset[i] = 0;
            self.audio_desc[i] = None;
            self.done(b, ch, 1, (a, f, next), length, f & (1 << 30) != 0)?;
        }
        Ok(sample)
    }
}

pub fn wav(frames: &[[i16; 2]]) -> Vec<u8> {
    let size = (frames.len() * 4) as u32;
    let mut out = Vec::with_capacity(size as usize + 44);
    out.extend(b"RIFF");
    out.extend((size + 36).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend(16u32.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(2u16.to_le_bytes());
    out.extend(AUDIO_RATE.to_le_bytes());
    out.extend((AUDIO_RATE * 4).to_le_bytes());
    out.extend(4u16.to_le_bytes());
    out.extend(16u16.to_le_bytes());
    out.extend(b"data");
    out.extend(size.to_le_bytes());
    for frame in frames {
        for sample in frame {
            out.extend(sample.to_le_bytes())
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    fn buzzer(clock: u32, timer: u32) -> Hardware {
        let mut hw = Hardware::default();
        for (address, value) in [
            (0x60004580, 73),
            (0x600190d0, clock),
            (0x600190a0, timer),
            (0x60019000, 4),
            (0x60019008, 512 << 4),
        ] {
            hw.regs.insert(address, value);
        }
        hw
    }
    #[test]
    fn fractional_i2s_clock_has_no_sample_boundary_drift() {
        let mut phase = Phase::default();
        phase.set_denominator(48000);
        let mut consumed = 0;
        for n in 1..=48000u64 {
            consumed += u64::from(phase.advance(44100));
            assert_eq!(consumed, n * 44100 / 48000);
        }
        assert_eq!(consumed, 44100);
        assert_eq!(phase.remainder, 0);
        phase.set_denominator(100);
        assert_eq!(phase.advance(50), 0);
        phase.set_denominator(200);
        assert_eq!(phase.advance(100), 1);
        assert_eq!(phase.remainder, 0);
    }
    #[test]
    fn i2s_fractional_registers_reconstruct_the_exact_source_ratio() {
        let mut hw = Hardware::default();
        for (a, v) in [
            (0x600045fc, 22),
            (0x60004558, 24),
            (0x6000455c, 25),
            (0x6000f024, 4),
            (0x6000f02c, (15 << 24) | (15 << 13)),
            (0x6000f054, 1 << 16),
            (0x6000f034, (2 << 27) | 113),
            (0x6000f03c, (1 << 18) | (108 << 9) | 167),
        ] {
            hw.regs.insert(a, v);
        }
        let format = hw.format(0).unwrap();
        assert_eq!(format.numerator, 44100 * format.denominator);
        let mut phase = Phase::default();
        phase.set_denominator(format.denominator * 48000);
        let total: u64 = (0..48000)
            .map(|_| u64::from(phase.advance(format.numerator)))
            .sum();
        assert_eq!(total, 44100);
        assert_eq!(phase.remainder, 0);
        hw.regs.insert(0x6000f03c, (1 << 27) | (167 << 9) | 274);
        let format = hw.format(0).unwrap();
        assert_eq!(format.numerator, 44100 * format.denominator);
    }
    #[test]
    fn ledc_tick_sel_and_global_sources_follow_esp32s3_registers() {
        for (source, divider, reference) in [
            (1, 20000, false),
            (2, 2000, false),
            (3, 10000, false),
            (1, 250, true),
        ] {
            let hw = buzzer(
                source,
                (divider << 4) | 10 | if reference { 1 << 24 } else { 0 },
            );
            let (hz, duty) = hw.buzzer();
            assert!(
                (hz - 1000.0).abs() < 0.001,
                "source {source}, reference {reference}: {hz}"
            );
            assert_eq!(duty, 0.5);
        }
        assert_eq!(buzzer(0, (10000 << 4) | 10).buzzer(), (0.0, 0.0));
    }
    #[test]
    fn keira_startup_buzzer_registers_produce_notes_not_subsonic_pwm() {
        for (timer, note) in [
            (0x0012a30a, 131.0),
            (0x0009518a, 262.0),
            (0x0004ab0a, 523.0),
            (0x00012aaa, 2093.0),
            (0x000254fa, 1047.0),
        ] {
            let (hz, _) = buzzer(3, timer).buzzer();
            assert!((hz - note).abs() < 0.1, "expected {note}, got {hz}");
        }
    }
}

#[cfg(test)]
mod refill_tests {
    use super::*;
    #[derive(Default)]
    struct Ram(std::collections::BTreeMap<u32, u32>);
    impl Bus for Ram {
        fn read(&mut self, a: u32) -> Result<u32> {
            Ok(self.0.get(&a).copied().unwrap_or(0))
        }
        fn write(&mut self, a: u32, v: u32) -> Result<()> {
            self.0.insert(a, v);
            Ok(())
        }
        fn irq(&mut self, _: u32, _: bool) -> Result<()> {
            Ok(())
        }
    }
    const A: u32 = 0x3fc9_0000;
    const B: u32 = 0x3fc9_0200;
    fn fill(ram: &mut Ram, buffer: u32, value: u32) {
        for word in 0..128 {
            ram.write(buffer + word * 4, value).unwrap();
        }
    }
    fn refill(h: &mut Hardware, ram: &mut Ram, buffer: u32) -> &'static str {
        match h.audio_refill(ram, 0, buffer, 512) {
            Refill::Play => "play",
            Refill::Skip => "skip",
            Refill::Wait => "wait",
        }
    }
    /// The DMA reaches the next buffer of a two-buffer ring; `samples` more I2S
    /// samples pass while it waits.
    fn wait(h: &mut Hardware, ram: &mut Ram, buffer: u32, samples: u32) -> &'static str {
        let mut result = refill(h, ram, buffer);
        for _ in 1..samples {
            result = refill(h, ram, buffer);
        }
        result
    }

    #[test]
    fn only_audio_the_firmware_wrote_plays() {
        let mut h = Hardware::default();
        let mut ram = Ram::default();
        fill(&mut ram, A, 0x1111_1111);
        fill(&mut ram, B, 0x2222_2222);
        assert_eq!(refill(&mut h, &mut ram, A), "play");
        assert_eq!(refill(&mut h, &mut ram, B), "play");
        // The firmware rewrote A only: A plays, then B would replay stale audio.
        fill(&mut ram, A, 0x3333_3333);
        assert_eq!(refill(&mut h, &mut ram, A), "play");
        assert_eq!(refill(&mut h, &mut ram, B), "wait");
        assert!(h.audio_paused(0));
        assert_eq!(wait(&mut h, &mut ram, B, REFILL_RECHECK), "wait");
        // The driver hands the firmware A again (the last buffer done); once it
        // holds new audio, stale B is skipped so that A plays next.
        fill(&mut ram, A, 0x4444_4444);
        assert_eq!(wait(&mut h, &mut ram, B, REFILL_RECHECK), "skip");
        assert!(!h.audio_paused(0));
        assert_eq!(refill(&mut h, &mut ram, A), "play");
        assert_eq!((h.audio_refill_waits, h.audio_refill_skips), (1, 1));
    }

    #[test]
    fn silence_plays_and_an_unchanged_ring_loops_after_patience() {
        let mut h = Hardware::default();
        let mut ram = Ram::default();
        for _ in 0..3 {
            assert_eq!(refill(&mut h, &mut ram, A), "play");
        }
        fill(&mut ram, A, 0x1111_1111);
        fill(&mut ram, B, 0x2222_2222);
        assert_eq!(refill(&mut h, &mut ram, A), "play");
        assert_eq!(refill(&mut h, &mut ram, B), "play");
        assert_eq!(refill(&mut h, &mut ram, A), "wait");
        // A ring the firmware never rewrites is audio it means to repeat: it
        // plays at the first recheck after REFILL_PATIENCE samples.
        let waited = (1..).find(|_| refill(&mut h, &mut ram, A) == "play").unwrap();
        assert!((REFILL_PATIENCE..REFILL_PATIENCE + REFILL_RECHECK).contains(&waited));
        for _ in 0..4 {
            assert_eq!(refill(&mut h, &mut ram, B), "play");
            assert_eq!(refill(&mut h, &mut ram, A), "play");
        }
        // New audio ends the loop: stale buffers wait again.
        fill(&mut ram, B, 0x5555_5555);
        assert_eq!(refill(&mut h, &mut ram, B), "play");
        assert_eq!(refill(&mut h, &mut ram, A), "wait");
    }
}
