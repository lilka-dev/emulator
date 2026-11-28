use anyhow::{Result, ensure};
#[cfg(not(target_arch = "wasm32"))]
use memmap2::{MmapMut as Mapping, MmapOptions};
#[cfg(target_arch = "wasm32")]
use shared::Mapping;
#[cfg(not(target_arch = "wasm32"))]
use std::fs::OpenOptions;
use std::{
    path::Path,
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
};

pub const WIDTH: usize = super::devices::display::WIDTH;
pub const HEIGHT: usize = super::devices::display::HEIGHT;
pub const AUDIO_CAPACITY: usize = 96_000;
pub const INPUT_MASK: u64 = (1u64 << 49) - 1;
const MAGIC: u32 = 0x4c49_4c32;
const HEADER: usize = 128;
const AUDIO: usize = HEADER + WIDTH * HEIGHT * 4;
const UART_HEAD: usize = AUDIO + AUDIO_CAPACITY * 4;
const UART_TAIL: usize = UART_HEAD + 4;
const UART_RX: usize = UART_TAIL + 4;
pub const UART_CAPACITY: usize = 16_384;
pub const SIZE: usize = UART_RX + UART_CAPACITY * 4;
const HEAD: usize = 8;
const TAIL: usize = 16;
const DROPPED: usize = 24;
const FRAME: usize = 32;
const INPUT: usize = 40;
const TIME: usize = 48;
const SPI: usize = 56;
const PIXELS: usize = 64;
const PCM: usize = 72;
const LCD_WINDOWS: usize = 80;
const LCD_CHANGED_WINDOWS: usize = 88;
const RUN: usize = 96;
const STATUS: usize = 100;
const PLAYED: usize = 104;
const UNDERRUNS: usize = 112;
const PLAYBACK_DROPPED: usize = 120;
/// Byte offsets of the protocol words, for a reader in another language.
pub const LAYOUT: &[(&str, usize)] = &[
    ("size", SIZE),
    ("width", WIDTH),
    ("height", HEIGHT),
    ("pixels", HEADER),
    ("uart_head", UART_HEAD),
    ("uart_tail", UART_TAIL),
    ("uart_rx", UART_RX),
    ("uart_capacity", UART_CAPACITY),
    ("audio", AUDIO),
    ("audio_capacity", AUDIO_CAPACITY),
    ("audio_head", HEAD),
    ("audio_tail", TAIL),
    ("frame", FRAME),
    ("input", INPUT),
    ("input_pins", 49),
    ("time", TIME),
    ("run", RUN),
    ("status", STATUS),
    ("played", PLAYED),
    ("underruns", UNDERRUNS),
    ("playback_dropped", PLAYBACK_DROPPED),
];

pub struct Ipc {
    map: Mapping,
}
#[derive(Clone, Copy, Debug)]
pub struct Stats {
    pub ticks: u64,
    pub spi_transfers: u64,
    pub lcd_pixels: u64,
    pub pcm_frames: u64,
    pub lcd_windows: u64,
    pub lcd_changed_windows: u64,
    pub dropped_frames: u64,
    pub status: u32,
    pub played_frames: u64,
    pub audio_underruns: u64,
    pub playback_dropped_frames: u64,
}
impl Ipc {
    #[cfg(not(target_arch = "wasm32"))]
    pub fn create(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        file.set_len(SIZE as u64)?;
        // SAFETY: A new private regular file has fixed SIZE; peers use only
        // atomic words through this protocol and keep its length unchanged.
        let this = Self {
            map: unsafe { MmapOptions::new().len(SIZE).map_mut(&file)? },
        };
        this.initialize();
        Ok(this)
    }
    /// The browser has no files to map: the page and the CPU threads share wasm
    /// memory, so one buffer per process stands in for the mapping and `path`
    /// only names it.
    #[cfg(target_arch = "wasm32")]
    pub fn create(_path: &Path) -> Result<Self> {
        let this = Self {
            map: Mapping::get(),
        };
        for offset in (0..SIZE).step_by(4) {
            this.u32(offset).store(0, Ordering::Relaxed);
        }
        this.initialize();
        Ok(this)
    }
    /// Address of the shared protocol buffer, for a page reading it in place.
    #[cfg(target_arch = "wasm32")]
    pub fn base() -> *const u8 {
        Mapping::get().as_ptr()
    }
    fn initialize(&self) {
        self.u32(4).store(2, Ordering::Relaxed);
        self.u64(INPUT).store(INPUT_MASK, Ordering::Relaxed);
        self.u32(RUN).store(1, Ordering::Relaxed);
        self.u32(0).store(MAGIC, Ordering::Release);
    }
    pub fn open(path: &Path) -> Result<Self> {
        #[cfg(not(target_arch = "wasm32"))]
        let this = {
            let file = OpenOptions::new().read(true).write(true).open(path)?;
            let meta = file.metadata()?;
            ensure!(
                meta.is_file() && meta.len() == SIZE as u64,
                "Invalid live mapping size"
            );
            // SAFETY: The owning launcher retains the fixed-size file until the
            // child exits. Every shared location is accessed atomically below.
            Self {
                map: unsafe { MmapOptions::new().len(SIZE).map_mut(&file)? },
            }
        };
        #[cfg(target_arch = "wasm32")]
        let this = {
            let _ = path;
            Self {
                map: Mapping::get(),
            }
        };
        ensure!(
            this.u32(0).load(Ordering::Acquire) == MAGIC
                && this.u32(4).load(Ordering::Relaxed) == 2,
            "Incompatible live mapping"
        );
        Ok(this)
    }
    fn u32(&self, offset: usize) -> &AtomicU32 {
        assert!(offset.is_multiple_of(4) && offset + 4 <= SIZE);
        // SAFETY: mmap is page-aligned; the checked offset is aligned/in-bounds.
        // No peer accesses this word through a non-atomic reference.
        unsafe { &*self.map.as_ptr().add(offset).cast::<AtomicU32>() }
    }
    fn u64(&self, offset: usize) -> &AtomicU64 {
        assert!(offset.is_multiple_of(8) && offset + 8 <= HEADER);
        // SAFETY: Same alignment/lifetime invariant as u32, within the header.
        unsafe { &*self.map.as_ptr().add(offset).cast::<AtomicU64>() }
    }
    /// Single producer (frontend), single consumer (QEMU under the BQL).
    pub fn send_uart(&self, bytes: &[u8]) -> usize {
        let head = self.u32(UART_HEAD).load(Ordering::Relaxed);
        let tail = self.u32(UART_TAIL).load(Ordering::Acquire);
        let count = bytes
            .len()
            .min(UART_CAPACITY.saturating_sub(head.wrapping_sub(tail) as usize));
        for (i, byte) in bytes[..count].iter().enumerate() {
            self.u32(UART_RX + (head.wrapping_add(i as u32) as usize % UART_CAPACITY) * 4)
                .store(u32::from(*byte), Ordering::Relaxed);
        }
        self.u32(UART_HEAD)
            .store(head.wrapping_add(count as u32), Ordering::Release);
        count
    }
    pub fn receive_uart(&self, bytes: &mut [u8]) -> usize {
        let tail = self.u32(UART_TAIL).load(Ordering::Relaxed);
        let head = self.u32(UART_HEAD).load(Ordering::Acquire);
        let count = bytes
            .len()
            .min(head.wrapping_sub(tail) as usize)
            .min(UART_CAPACITY);
        for (i, byte) in bytes[..count].iter_mut().enumerate() {
            *byte = self
                .u32(UART_RX + (tail.wrapping_add(i as u32) as usize % UART_CAPACITY) * 4)
                .load(Ordering::Relaxed) as u8;
        }
        self.u32(UART_TAIL)
            .store(tail.wrapping_add(count as u32), Ordering::Release);
        count
    }
    pub fn push_audio(&self, frames: &[[i16; 2]]) -> usize {
        let head = self.u64(HEAD).load(Ordering::Relaxed);
        let tail = self.u64(TAIL).load(Ordering::Acquire);
        let available = AUDIO_CAPACITY
            .saturating_sub(head.wrapping_sub(tail).min(AUDIO_CAPACITY as u64) as usize);
        let count = frames.len().min(available);
        for (i, frame) in frames[..count].iter().enumerate() {
            let slot = head.wrapping_add(i as u64) % AUDIO_CAPACITY as u64;
            let bits = frame[0] as u16 as u32 | ((frame[1] as u16 as u32) << 16);
            self.u32(AUDIO + slot as usize * 4)
                .store(bits, Ordering::Relaxed);
        }
        self.u64(HEAD)
            .store(head.wrapping_add(count as u64), Ordering::Release);
        self.u64(DROPPED)
            .fetch_add((frames.len() - count) as u64, Ordering::Relaxed);
        count
    }
    pub fn drain_audio(&self, output: &mut Vec<[i16; 2]>) {
        let tail = self.u64(TAIL).load(Ordering::Relaxed);
        let head = self.u64(HEAD).load(Ordering::Acquire);
        let count = head.wrapping_sub(tail).min(AUDIO_CAPACITY as u64);
        output.clear();
        output.reserve(count as usize);
        for i in 0..count {
            let bits = self
                .u32(AUDIO + ((tail.wrapping_add(i) % AUDIO_CAPACITY as u64) as usize) * 4)
                .load(Ordering::Relaxed);
            output.push([bits as u16 as i16, (bits >> 16) as u16 as i16]);
        }
        self.u64(TAIL)
            .store(tail.wrapping_add(count), Ordering::Release);
    }
    pub fn publish_rgb(&self, rgb: &[u8]) {
        assert_eq!(rgb.len(), WIDTH * HEIGHT * 3);
        self.u64(FRAME).fetch_add(1, Ordering::AcqRel);
        std::sync::atomic::fence(Ordering::Release);
        for (i, pixel) in rgb.chunks_exact(3).enumerate() {
            let bits = pixel[0] as u32
                | ((pixel[1] as u32) << 8)
                | ((pixel[2] as u32) << 16)
                | 0xff00_0000;
            self.u32(HEADER + i * 4).store(bits, Ordering::Relaxed);
        }
        self.u64(FRAME).fetch_add(1, Ordering::Release);
    }
    pub fn publish_rgb565(&self, pixels: &[u16], visible: bool, bgr: bool) {
        assert_eq!(pixels.len(), WIDTH * HEIGHT);
        self.u64(FRAME).fetch_add(1, Ordering::AcqRel);
        std::sync::atomic::fence(Ordering::Release);
        for (i, &value) in pixels.iter().enumerate() {
            let value = if visible { value } else { 0 };
            let r = ((value >> 11) & 31) as u32 * 255 / 31;
            let g = ((value >> 5) & 63) as u32 * 255 / 63;
            let b = (value & 31) as u32 * 255 / 31;
            let (r, b) = if bgr { (b, r) } else { (r, b) };
            self.u32(HEADER + i * 4)
                .store(r | (g << 8) | (b << 16) | 0xff00_0000, Ordering::Relaxed);
        }
        self.u64(FRAME).fetch_add(1, Ordering::Release);
    }
    pub fn read_rgba(&self, previous: &mut u64, output: &mut Vec<u8>) -> bool {
        let seq = self.u64(FRAME).load(Ordering::Acquire);
        if seq == *previous || seq & 1 != 0 {
            return false;
        }
        output.clear();
        output.reserve(WIDTH * HEIGHT * 4);
        for i in 0..WIDTH * HEIGHT {
            output.extend_from_slice(
                &self
                    .u32(HEADER + i * 4)
                    .load(Ordering::Relaxed)
                    .to_le_bytes(),
            );
        }
        std::sync::atomic::fence(Ordering::SeqCst);
        if self.u64(FRAME).load(Ordering::Acquire) != seq {
            return false;
        }
        *previous = seq;
        true
    }
    pub fn input_levels(&self) -> u64 {
        self.u64(INPUT).load(Ordering::Acquire)
    }
    pub fn set_pin(&self, pin: u8, high: bool) {
        assert!(pin < 49);
        if high {
            self.u64(INPUT).fetch_or(1u64 << pin, Ordering::Release);
        } else {
            self.u64(INPUT).fetch_and(!(1u64 << pin), Ordering::Release);
        }
    }
    pub fn release_inputs(&self) {
        self.u64(INPUT).store(INPUT_MASK, Ordering::Release);
    }
    pub fn request_running(&self, running: bool) {
        self.u32(RUN).store(running as u32, Ordering::Release);
    }
    pub fn wants_running(&self) -> bool {
        self.u32(RUN).load(Ordering::Acquire) != 0
    }
    pub fn set_status(&self, status: u32) {
        self.u32(STATUS).store(status, Ordering::Release);
    }
    pub fn publish_stats(&self, ticks: u64, spi: u64, pixels: u64, pcm: u64) {
        for (offset, value) in [(SPI, spi), (PIXELS, pixels), (PCM, pcm)] {
            self.u64(offset).store(value, Ordering::Relaxed);
        }
        self.u64(TIME).store(ticks, Ordering::Release);
    }
    pub fn publish_lcd_windows(&self, completed: u64, changed: u64) {
        self.u64(LCD_WINDOWS).store(completed, Ordering::Relaxed);
        self.u64(LCD_CHANGED_WINDOWS)
            .store(changed, Ordering::Relaxed);
    }
    pub fn publish_playback(&self, played: u64, underruns: u64, dropped: u64) {
        self.u64(PLAYED).store(played, Ordering::Release);
        self.u64(UNDERRUNS).store(underruns, Ordering::Release);
        self.u64(PLAYBACK_DROPPED).store(dropped, Ordering::Release);
    }
    pub fn stats(&self) -> Stats {
        Stats {
            ticks: self.u64(TIME).load(Ordering::Acquire),
            spi_transfers: self.u64(SPI).load(Ordering::Relaxed),
            lcd_pixels: self.u64(PIXELS).load(Ordering::Relaxed),
            pcm_frames: self.u64(PCM).load(Ordering::Relaxed),
            lcd_windows: self.u64(LCD_WINDOWS).load(Ordering::Relaxed),
            lcd_changed_windows: self.u64(LCD_CHANGED_WINDOWS).load(Ordering::Relaxed),
            dropped_frames: self.u64(DROPPED).load(Ordering::Relaxed),
            status: self.u32(STATUS).load(Ordering::Acquire),
            played_frames: self.u64(PLAYED).load(Ordering::Acquire),
            audio_underruns: self.u64(UNDERRUNS).load(Ordering::Acquire),
            playback_dropped_frames: self.u64(PLAYBACK_DROPPED).load(Ordering::Acquire),
        }
    }
}

#[cfg(target_arch = "wasm32")]
mod shared {
    use std::sync::OnceLock;
    /// One zeroed, 8-byte aligned buffer of `SIZE` bytes that lives for the process.
    #[derive(Clone, Copy)]
    pub struct Mapping(*mut u8);
    // SAFETY: The buffer is never freed and is only accessed through atomics.
    unsafe impl Send for Mapping {}
    unsafe impl Sync for Mapping {}
    impl Mapping {
        pub fn get() -> Self {
            static BUFFER: OnceLock<usize> = OnceLock::new();
            let address = *BUFFER.get_or_init(|| {
                let words = vec![0u64; super::SIZE.div_ceil(8)].leak();
                words.as_mut_ptr() as usize
            });
            Self(address as *mut u8)
        }
        pub fn as_ptr(&self) -> *const u8 {
            self.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uart_backpressure_wrap_and_counter_rollover() -> Result<()> {
        let path = path();
        let sender = Ipc::create(&path)?;
        let receiver = Ipc::open(&path)?;
        sender.u32(UART_HEAD).store(u32::MAX - 7, Ordering::Relaxed);
        sender.u32(UART_TAIL).store(u32::MAX - 7, Ordering::Relaxed);
        let data: Vec<_> = (0..UART_CAPACITY + 100).map(|i| (i % 251) as u8).collect();
        assert_eq!(sender.send_uart(&data), UART_CAPACITY);
        assert_eq!(sender.send_uart(b"blocked"), 0);
        let mut output = vec![0; 97];
        assert_eq!(receiver.receive_uart(&mut output), 97);
        assert_eq!(&output, &data[..97]);
        assert_eq!(sender.send_uart(&data[UART_CAPACITY..]), 97);
        output.resize(UART_CAPACITY, 0);
        assert_eq!(receiver.receive_uart(&mut output), UART_CAPACITY);
        assert_eq!(&output, &data[97..UART_CAPACITY + 97]);
        assert_eq!(receiver.receive_uart(&mut output), 0);
        drop(sender);
        drop(receiver);
        std::fs::remove_file(path)?;
        Ok(())
    }
    #[test]
    fn uart_concurrent_peers_preserve_every_byte() -> Result<()> {
        let path = path();
        let sender = Ipc::create(&path)?;
        let receiver = Ipc::open(&path)?;
        let data: Vec<_> = (0..200_000).map(|i| (i % 251) as u8).collect();
        let expected = data.clone();
        let producer = std::thread::spawn(move || {
            let mut at = 0;
            while at < data.len() {
                at += sender.send_uart(&data[at..]);
                std::thread::yield_now();
            }
        });
        let mut output = Vec::new();
        let mut buffer = [0; 127];
        while output.len() < expected.len() {
            let n = receiver.receive_uart(&mut buffer);
            output.extend_from_slice(&buffer[..n]);
            std::thread::yield_now();
        }
        producer.join().unwrap();
        assert_eq!(output, expected);
        drop(receiver);
        std::fs::remove_file(path)?;
        Ok(())
    }
    #[test]
    fn lcd_counters_cross_mapping_without_changing_controls() -> Result<()> {
        let path = path();
        let writer = Ipc::create(&path)?;
        let reader = Ipc::open(&path)?;
        writer.publish_lcd_windows(123, 97);
        writer.publish_stats(240_000_000, 42, 49_000, 48_000);
        let stats = reader.stats();
        assert_eq!((stats.lcd_windows, stats.lcd_changed_windows), (123, 97));
        assert_eq!(stats.ticks, 240_000_000);
        assert_eq!(stats.status, 0);
        assert!(reader.wants_running());
        reader.request_running(false);
        assert!(!writer.wants_running());
        drop(reader);
        drop(writer);
        std::fs::remove_file(path)?;
        Ok(())
    }
    #[test]
    fn rgb565_publication_preserves_channels_and_panel_visibility() -> Result<()> {
        let path = path();
        let ipc = Ipc::create(&path)?;
        let mut pixels = vec![0; WIDTH * HEIGHT];
        pixels[..4].copy_from_slice(&[0xf800, 0x07e0, 0x001f, 0xffff]);
        let mut sequence = 0;
        let mut rgba = Vec::new();
        for (visible, bgr, expected) in [
            (
                true,
                false,
                [
                    255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
                ],
            ),
            (
                true,
                true,
                [
                    0, 0, 255, 255, 0, 255, 0, 255, 255, 0, 0, 255, 255, 255, 255, 255,
                ],
            ),
            (
                false,
                false,
                [0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255],
            ),
        ] {
            ipc.publish_rgb565(&pixels, visible, bgr);
            assert!(ipc.read_rgba(&mut sequence, &mut rgba));
            assert_eq!(&rgba[..16], &expected);
        }
        drop(ipc);
        std::fs::remove_file(path)?;
        Ok(())
    }
    fn path() -> std::path::PathBuf {
        static ID: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "lilka-live-{}-{}.ipc",
            std::process::id(),
            ID.fetch_add(1, Ordering::Relaxed)
        ))
    }
    #[test]
    fn mapped_peers_keep_pcm_order_at_wrap_and_report_overflow() -> Result<()> {
        let path = path();
        let writer = Ipc::create(&path)?;
        let reader = Ipc::open(&path)?;
        let mut output = Vec::new();
        let data: Vec<_> = (0..AUDIO_CAPACITY + 3)
            .map(|i| [i as i16, !(i as i16)])
            .collect();
        assert_eq!(writer.push_audio(&data), AUDIO_CAPACITY);
        reader.drain_audio(&mut output);
        assert_eq!(output, &data[..AUDIO_CAPACITY]);
        assert_eq!(reader.stats().dropped_frames, 3);
        assert_eq!(writer.push_audio(&data[..11]), 11);
        reader.drain_audio(&mut output);
        assert_eq!(output, &data[..11]);
        reader.set_pin(41, false);
        assert_eq!(writer.input_levels() & (1 << 41), 0);
        reader.request_running(false);
        assert!(!writer.wants_running());
        drop(reader);
        drop(writer);
        std::fs::remove_file(path)?;
        Ok(())
    }
    #[test]
    fn concurrently_published_frames_are_never_accepted_torn() -> Result<()> {
        let path = path();
        let writer = Ipc::create(&path)?;
        let reader = Ipc::open(&path)?;
        let thread = std::thread::spawn(move || {
            for color in 1..32 {
                writer.publish_rgb(&vec![color; WIDTH * HEIGHT * 3]);
            }
        });
        let mut seq = 0;
        let mut rgba = Vec::new();
        let mut accepted = 0;
        while !thread.is_finished() {
            if reader.read_rgba(&mut seq, &mut rgba) {
                accepted += 1;
                assert!(
                    rgba.chunks_exact(4)
                        .all(|p| p == [rgba[0], rgba[0], rgba[0], 255])
                );
            }
        }
        thread.join().unwrap();
        if reader.read_rgba(&mut seq, &mut rgba) {
            accepted += 1;
            assert!(rgba.chunks_exact(4).all(|p| p == [31, 31, 31, 255]));
        }
        assert!(accepted > 0);
        drop(reader);
        std::fs::remove_file(path)?;
        Ok(())
    }
}
