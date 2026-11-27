mod net;
use lilka_board::bus::{Access, Bus, Device, MemoryFault};
use lilka_board::{
    adapter::BoardDevice,
    devices::{display, sd::SdCard},
};
use std::{
    ffi::c_void,
    path::PathBuf,
    sync::{Arc, mpsc},
    thread::JoinHandle,
};

struct Frame {
    pixels: Vec<u16>,
    visible: bool,
    bgr: bool,
}
struct FramePublisher {
    pending: Option<mpsc::SyncSender<Frame>>,
    available: mpsc::Receiver<Frame>,
    recycle: mpsc::SyncSender<Frame>,
    worker: Option<JoinHandle<()>>,
}
impl FramePublisher {
    fn new(ipc: Arc<lilka_board::live::Ipc>) -> Self {
        let (pending, frames) = mpsc::sync_channel::<Frame>(1);
        let (recycle, available) = mpsc::sync_channel(2);
        for _ in 0..2 {
            recycle
                .send(Frame {
                    pixels: Vec::with_capacity(display::WIDTH * display::HEIGHT),
                    visible: false,
                    bgr: false,
                })
                .unwrap();
        }
        let returned = recycle.clone();
        let worker = std::thread::spawn(move || {
            while let Ok(frame) = frames.recv() {
                ipc.publish_rgb565(&frame.pixels, frame.visible, frame.bgr);
                let _ = returned.try_send(frame);
            }
        });
        Self {
            pending: Some(pending),
            available,
            recycle,
            worker: Some(worker),
        }
    }
    fn publish(&self, display: &display::Display) {
        let Ok(mut frame) = self.available.try_recv() else {
            return;
        };
        (frame.visible, frame.bgr) = display.snapshot_rgb565(&mut frame.pixels);
        if let Err(error) = self.pending.as_ref().unwrap().try_send(frame) {
            let frame = match error {
                mpsc::TrySendError::Full(frame) | mpsc::TrySendError::Disconnected(frame) => frame,
            };
            let _ = self.recycle.try_send(frame);
        }
    }
}
impl Drop for FramePublisher {
    fn drop(&mut self) {
        self.pending.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

type Read = unsafe extern "C" fn(*mut c_void, u32, *mut u8, usize) -> i32;
type Write = unsafe extern "C" fn(*mut c_void, u32, *const u8, usize) -> i32;
struct Memory {
    opaque: *mut c_void,
    read: Read,
    write: Write,
}
impl Bus for Memory {
    fn read(&mut self, address: u32, width: u8, access: Access) -> Result<u32, MemoryFault> {
        if !matches!(width, 1 | 2 | 4) {
            return Err(MemoryFault { address, access });
        }
        let mut bytes = [0; 4];
        self.read_bytes(address, &mut bytes[..usize::from(width)])?;
        Ok(u32::from_le_bytes(bytes))
    }
    fn write(&mut self, address: u32, width: u8, value: u32) -> Result<(), MemoryFault> {
        if !matches!(width, 1 | 2 | 4) {
            return Err(MemoryFault {
                address,
                access: Access::Write,
            });
        }
        self.write_bytes(address, &value.to_le_bytes()[..usize::from(width)])
    }
    fn read_bytes(&mut self, address: u32, bytes: &mut [u8]) -> Result<(), MemoryFault> {
        // SAFETY: QEMU callback is live for this board; this writable slice is
        // valid for exactly len bytes, and the callback cannot reenter MMIO.
        if unsafe { (self.read)(self.opaque, address, bytes.as_mut_ptr(), bytes.len()) } == 0 {
            Ok(())
        } else {
            Err(MemoryFault {
                address,
                access: Access::Read,
            })
        }
    }
    fn write_bytes(&mut self, address: u32, bytes: &[u8]) -> Result<(), MemoryFault> {
        // SAFETY: Same callback lifetime and no-reentry invariant as read_bytes;
        // QEMU copies the immutable input before returning.
        if unsafe { (self.write)(self.opaque, address, bytes.as_ptr(), bytes.len()) } == 0 {
            Ok(())
        } else {
            Err(MemoryFault {
                address,
                access: Access::Write,
            })
        }
    }
}
pub struct Bridge {
    keira_vfs16: bool,
    #[cfg(feature = "wifi-experimental")]
    wifi: Option<wifi::Radio>,
    board: BoardDevice,
    sensors: lilka_board::sensors::Sensors,
    memory: Memory,
    pcm: Vec<[i16; 2]>,
    audio_chunk: Vec<[i16; 2]>,
    pcm_frames: u64,
    capture_pcm: bool,
    live: Option<Arc<lilka_board::live::Ipc>>,
    frame_publisher: Option<FramePublisher>,
    live_frame_tick: u64,
    scripted_levels: u64,
    now: u64,
    inputs: std::collections::VecDeque<(u64, u8, bool)>,
    warm_start: Option<(u64, std::time::Instant)>,
    warm_result: Option<(f64, f64)>,
    system_resets: u64,
    prepare_audio_events: bool,
}
impl Bridge {
    fn advance(&mut self, now: u64) {
        self.now = now;
        #[cfg(feature = "wifi-experimental")]
        if let Some(wifi) = &mut self.wifi {
            wifi.advance(now, &mut self.memory);
        }
        if self.warm_start.is_none() && now >= 17 * 240_000_000 {
            self.warm_start = Some((now, std::time::Instant::now()));
        }
        if self.warm_result.is_none() && now >= 23 * 240_000_000 {
            if let Some((start, wall)) = self.warm_start {
                self.warm_result = Some((
                    (now - start) as f64 / 240_000_000.0,
                    wall.elapsed().as_secs_f64(),
                ));
            }
        }
        while self.inputs.front().is_some_and(|e| e.0 <= now) {
            let (_, pin, pressed) = self.inputs.pop_front().unwrap();
            if pressed {
                self.scripted_levels &= !(1u64 << pin);
            } else {
                self.scripted_levels |= 1u64 << pin;
            }
        }
        self.board.state.gpio.external = self.scripted_levels
            & self
                .live
                .as_ref()
                .map_or(u64::MAX, |live| live.input_levels());
        self.board.advance(now, &mut self.memory);
        self.audio_chunk.clear();
        self.audio_chunk
            .extend(self.board.state.hardware.samples.drain(..));
        self.pcm_frames += self.audio_chunk.len() as u64;
        if self.capture_pcm {
            self.pcm.extend_from_slice(&self.audio_chunk);
        }
        if let Some(live) = &self.live {
            live.push_audio(&self.audio_chunk);
            live.publish_lcd_windows(
                self.board.state.devices.display.completed_large_windows,
                self.board.state.devices.display.changed_large_windows,
            );
            live.publish_stats(
                now,
                self.board.state.transfers as u64,
                self.board.state.devices.display.pixels_written as u64,
                self.pcm_frames,
            );
            if self.live_frame_tick == 0 || now.saturating_sub(self.live_frame_tick) >= 8_000_000 {
                self.frame_publisher
                    .as_ref()
                    .unwrap()
                    .publish(&self.board.state.devices.display);
                self.live_frame_tick = now;
            }
        }
    }
    fn dump(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let out = PathBuf::from(std::env::var("LILKA_QEMU_OUTPUT_DIR")?);
        std::fs::create_dir_all(&out)?;
        self.board.state.devices.sd.flush()?;
        if self.board.state.devices.sd.is_folder_backed() {
            std::fs::write(out.join("sd-folder-flushed"), b"ok\n")?;
        }
        #[cfg(feature = "wifi-experimental")]
        let wifi_stats = self
            .wifi
            .as_ref()
            .map(|wifi| wifi.diagnostics(&mut self.memory));
        #[cfg(not(feature = "wifi-experimental"))]
        let wifi_stats: Option<serde_json::Value> = None;
        let s = &self.board.state;
        let mut ppm = format!("P6\n{} {}\n255\n", display::WIDTH, display::HEIGHT).into_bytes();
        ppm.extend(s.devices.display.rgb());
        std::fs::write(out.join("display.ppm"), ppm)?;
        let mut audio = Vec::with_capacity(self.pcm.len() * 4);
        for frame in &self.pcm {
            for sample in frame {
                audio.extend(sample.to_le_bytes());
            }
        }
        std::fs::write(out.join("audio-s16le-48000-stereo.pcm"), audio)?;
        std::fs::write(
            out.join("board.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "ticks": self.now, "guest_seconds": self.now as f64 / 240_000_000.0,
                "spi_transfers": s.transfers, "shared_spi_select_transfers": s.devices.shared_select_transfers, "display_transfers": s.devices.display_transfers,
                "sd_transfers": s.devices.sd_transfers, "lcd_pixels": s.devices.display.pixels_written,
                "lcd_completed_large_windows": s.devices.display.completed_large_windows,
                "lcd_changed_large_windows": s.devices.display.changed_large_windows,
                "sd_reads": s.devices.sd.blocks_read, "sd_writes": s.devices.sd.blocks_written,
                "sd_file_read_batches": s.devices.sd.file_read_batches,
                "sd_file_cached_sector_reads": s.devices.sd.file_cached_sector_reads,
                "sd_io_error": s.devices.sd.io_error,
                "wifi": wifi_stats,
                "gpio_output": s.gpio.output, "gpio_enable": s.gpio.enable,
                "nonzero_lcd_ram_pixels": (0..display::HEIGHT).flat_map(|y| (0..display::WIDTH).map(move |x| (x,y))).filter(|&(x,y)| s.devices.display.pixel(x,y) != 0).count(),
                "i2s_frames": s.hardware.i2s_frames, "buzzer_frames": s.hardware.buzzer_frames,
                "dma_descriptors": s.hardware.dma_descriptors, "pcm_frames": self.pcm_frames,
                "dma_trace": s.hardware.dma_trace,
                "audio_completion_overflows": s.hardware.audio_completion_overflows,
                "errors": s.errors, "display_diagnostics": s.devices.display.diagnostics(),
                "pending_inputs": self.inputs.len(),
                "system_resets": self.system_resets,
                "warm_window_virtual_and_wall_seconds": self.warm_result,
                "limitation": "Experimental backend: clocks/CPU ordering not yet calibrated against Lilka workload."
            }))?,
        )?;
        Ok(())
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn lilka_board_create(opaque: *mut c_void, read: Read, write: Write) -> *mut Bridge {
    #[cfg(feature = "wifi-experimental")]
    let wifi = match std::env::var("LILKA_QEMU_WIFI_SSID")
        .ok()
        .map(|ssid| wifi::Radio::new(&ssid))
        .transpose()
    {
        Ok(wifi) => wifi,
        Err(error) => {
            eprintln!("Lilka Wi-Fi: {error}");
            return std::ptr::null_mut();
        }
    };
    #[cfg(not(feature = "wifi-experimental"))]
    if std::env::var_os("LILKA_QEMU_WIFI_SSID").is_some() {
        eprintln!("Lilka Wi-Fi requires the experimental radio build");
        return std::ptr::null_mut();
    }
    let mut board = BoardDevice::default();
    board.state.hardware.audio_completion_queue =
        std::env::var_os("LILKA_QEMU_AUDIO_COMPLETIONS").is_some();
    if std::env::var_os("LILKA_QEMU_TRACE_DMA").is_some() {
        board.state.hardware.dma_trace = Some(Vec::with_capacity(65536));
    }
    board.state.gpio.strap = 4;
    board.state.devices.strict_shared_selects = std::env::var_os("LILKA_QEMU_STRICT_SPI").is_some();
    if let Some(path) = std::env::var_os("LILKA_QEMU_SD_FOLDER") {
        match SdCard::from_folder(std::path::Path::new(&path)) {
            Ok(sd) => board.state.devices.sd = sd,
            Err(error) => {
                eprintln!("Lilka SD folder: {error}");
                return std::ptr::null_mut();
            }
        }
    } else if let Ok(path) = std::env::var("LILKA_QEMU_SD_IMAGE") {
        let loaded = if std::env::var_os("LILKA_QEMU_SD_FILE_BACKED").is_some() {
            SdCard::from_file(std::path::Path::new(&path), true).map_err(|e| e.to_string())
        } else {
            std::fs::read(path)
                .map_err(|e| e.to_string())
                .and_then(|b| SdCard::from_image(b).map_err(|e| e.to_string()))
        };
        match loaded {
            Ok(sd) => board.state.devices.sd = sd,
            Err(e) => {
                eprintln!("Lilka SD: {e}");
                return std::ptr::null_mut();
            }
        }
    }
    let mut sensors = lilka_board::sensors::Sensors::default();
    sensors.input_uv[0][2] = 1_000_000;
    let mut inputs: Vec<(u64, u8, bool)> = match std::env::var("LILKA_QEMU_INPUTS") {
        Ok(s) => match serde_json::from_str(&s) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Lilka inputs: {e}");
                return std::ptr::null_mut();
            }
        },
        Err(_) => Vec::new(),
    };
    if inputs.iter().any(|e| e.1 >= 49) {
        return std::ptr::null_mut();
    }
    inputs.sort_by_key(|e| e.0);
    let live = match std::env::var_os("LILKA_QEMU_LIVE_FILE") {
        Some(path) => match lilka_board::live::Ipc::open(std::path::Path::new(&path)) {
            Ok(live) => Some(Arc::new(live)),
            Err(error) => {
                eprintln!("Lilka live mapping: {error}");
                return std::ptr::null_mut();
            }
        },
        None => None,
    };
    let capture_pcm = live.is_none() || std::env::var_os("LILKA_QEMU_CAPTURE_PCM").is_some();
    let frame_publisher = live.as_ref().map(|live| FramePublisher::new(live.clone()));
    let scripted_levels = board.state.gpio.external;
    let keira_vfs16 = std::env::var("LILKA_QEMU_KEIRA_VFS16").as_deref() == Ok("1");
    let mut memory = Memory {
        opaque,
        read,
        write,
    };
    if keira_vfs16 && memory.write_bytes(0x3fde_ff00, &[0; 64]).is_err() {
        eprintln!("Lilka: CPU backend has no Keira compatibility RAM");
        return std::ptr::null_mut();
    }
    Box::into_raw(Box::new(Bridge {
        keira_vfs16,
        #[cfg(feature = "wifi-experimental")]
        wifi,
        board,
        sensors,
        memory,
        pcm: Vec::new(),
        audio_chunk: Vec::new(),
        pcm_frames: 0,
        capture_pcm,
        live,
        frame_publisher,
        live_frame_tick: 0,
        scripted_levels,
        now: 0,
        inputs: inputs.into(),
        warm_start: None,
        warm_result: None,
        system_resets: 0,
        prepare_audio_events: std::env::var_os("LILKA_QEMU_SERVICE_CLOCK").is_some(),
    }))
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lilka_board_peripheral_reset(
    p: *mut Bridge,
    mask0: u32,
    mask1: u32,
    now: u64,
) {
    // SAFETY: BQL serializes this callback with all other Rust operations.
    // Native SPI banks belong to C and cannot alias the borrowed Rust model.
    let b = unsafe { &mut *p };
    b.advance(now);
    b.board.reset_peripherals(mask0, mask1);
    if mask0 & (1 << 28) != 0 {
        let input_uv = b.sensors.input_uv;
        b.sensors = lilka_board::sensors::Sensors::default();
        b.sensors.input_uv = input_uv;
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lilka_board_reset(p: *mut Bridge, now: u64) {
    // SAFETY: QEMU invokes a peripheral reset with vCPUs quiescent and BQL
    // held. The video worker owns only snapshots, never board references.
    let b = unsafe { &mut *p };
    #[cfg(feature = "wifi-experimental")]
    if let Some(wifi) = &mut b.wifi {
        wifi.reset();
    }
    b.advance(now);
    b.frame_publisher.take();
    b.board.reset(now);
    if b.keira_vfs16 && b.memory.write_bytes(0x3fde_ff00, &[0; 64]).is_err() {
        b.board
            .state
            .errors
            .push("Keira VFS compatibility RAM reset failed".into());
    }
    let input_uv = b.sensors.input_uv;
    b.sensors = lilka_board::sensors::Sensors::default();
    b.sensors.input_uv = input_uv;
    b.live_frame_tick = 0;
    b.frame_publisher = b
        .live
        .as_ref()
        .map(|live| FramePublisher::new(live.clone()));
    b.system_resets += 1;
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lilka_board_spi_sync(
    p: *mut Bridge,
    index: u32,
    words: *mut u32,
    into_model: u32,
) -> i32 {
    // SAFETY: QEMU holds BQL and serializes every Rust callback. Other guest
    // CPUs can access only separate C-owned shadow banks, never this Box.
    // The caller's separate stack buffer contains exactly 64 words
    // and cannot overlap this live Box or be accessed by another thread.
    let b = unsafe { &mut *p };
    let Some(registers) = b.board.spi_register_bank_mut(index as usize) else {
        return -1;
    };
    // SAFETY: Both arrays have 64 initialized u32 elements, aligned and
    // disjoint; no reference escapes this synchronous, serialized copy.
    unsafe {
        if into_model != 0 {
            std::ptr::copy_nonoverlapping(words, registers.as_mut_ptr(), 64);
        } else {
            std::ptr::copy_nonoverlapping(registers.as_ptr(), words, 64);
        }
    }
    0
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lilka_board_read(
    p: *mut Bridge,
    address: u32,
    width: u8,
    now: u64,
) -> u32 {
    // SAFETY: C retains this live Box pointer and serializes access under BQL.
    let b = unsafe { &mut *p };
    #[cfg(feature = "wifi-experimental")]
    if wifi::Radio::handles(address)
        && let Some(wifi) = &mut b.wifi
    {
        return wifi.read(address & !3, now) >> ((address & 3) * 8);
    }
    if (0x60008800..0x60008858).contains(&address) {
        return b
            .sensors
            .read(address - 0x60008800, width, now)
            .unwrap_or(0);
    }
    b.board.read(address, width, now).unwrap_or(0)
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lilka_board_write(
    p: *mut Bridge,
    address: u32,
    width: u8,
    value: u32,
    now: u64,
) {
    // SAFETY: Live Box pointer, BQL serialization, RAM-only callbacks.
    let b = unsafe { &mut *p };
    #[cfg(feature = "wifi-experimental")]
    if wifi::Radio::handles(address)
        && let Some(wifi) = &mut b.wifi
    {
        let aligned = address & !3;
        let value = if width == 4 {
            value
        } else {
            let shift = (address & 3) * 8;
            let mask = ((1u32 << (u32::from(width) * 8)) - 1) << shift;
            (wifi.read(aligned, now) & !mask) | ((value << shift) & mask)
        };
        wifi.write(aligned, value, now);
        wifi.advance(now, &mut b.memory);
        return;
    }
    if (0x60008800..0x60008858).contains(&address) {
        b.sensors.write(address - 0x60008800, width, value, now);
        return;
    }
    b.board
        .write_with_bus(address, width, value, now, &mut b.memory);
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lilka_board_tick(p: *mut Bridge, now: u64) {
    // SAFETY: Timer callback also holds BQL and cannot overlap device callbacks.
    unsafe { &mut *p }.advance(now);
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lilka_board_next_event(p: *mut Bridge, now: u64) -> u64 {
    // SAFETY: C serializes this query and the optional RAM-only descriptor
    // peek with all other model callbacks under BQL. No borrow escapes.
    let b = unsafe { &mut *p };
    if b.prepare_audio_events {
        b.board.prepare_audio_events(&mut b.memory);
    }
    b.board.next_event(now).unwrap_or(u64::MAX)
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lilka_board_audio_irq_pending(p: *const Bridge) -> u32 {
    // SAFETY: QEMU retains the live board pointer and serializes this read
    // with all model callbacks under BQL, including peripheral resets.
    u32::from(unsafe { &*p }.board.state.hardware.audio_irq_pending())
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lilka_board_irq(p: *mut Bridge, high: u32) -> u64 {
    // SAFETY: Same serialized live-pointer contract; high is checked below.
    let b = unsafe { &mut *p };
    if high > 1 {
        return 0;
    }
    let bits = (b.board.state.interrupts >> (high * 64)) as u64;
    #[cfg(feature = "wifi-experimental")]
    let bits = bits | u64::from(high == 0 && b.wifi.as_ref().is_some_and(|wifi| wifi.irq()));
    bits
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lilka_board_wants_run(p: *mut Bridge) -> u32 {
    // SAFETY: QEMU serializes real/virtual timers and MMIO callbacks under BQL.
    unsafe { &*p }
        .live
        .as_ref()
        .is_none_or(|live| live.wants_running()) as u32
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lilka_board_live_status(p: *mut Bridge, status: u32) {
    // SAFETY: Same live Box and exclusive callback boundary as wants_run.
    if let Some(live) = &unsafe { &*p }.live {
        live.set_status(status);
    }
}
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lilka_board_finish(p: *mut Bridge) {
    // SAFETY: Called once at process exit, after execution stops, before dlclose.
    let mut b = unsafe { Box::from_raw(p) };
    let final_frames: Vec<_> = b.board.state.hardware.samples.drain(..).collect();
    b.pcm_frames += final_frames.len() as u64;
    if b.capture_pcm {
        b.pcm.extend_from_slice(&final_frames);
    }
    if let Some(live) = &b.live {
        live.push_audio(&final_frames);
    }
    let dumped = b.dump();
    if let Err(e) = &dumped {
        eprintln!("Lilka output: {e}");
    }
    // Only after the SD card is written back to its folder: the browser page
    // waits for this status before it saves the folder and reloads.
    if let Some(live) = &b.live {
        live.set_status(if dumped.is_ok() { 3 } else { 4 });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    unsafe extern "C" fn no_read(_: *mut c_void, _: u32, _: *mut u8, _: usize) -> i32 {
        -1
    }
    unsafe extern "C" fn no_write(_: *mut c_void, _: u32, _: *const u8, _: usize) -> i32 {
        -1
    }
    #[test]
    fn spi_bank_sync_roundtrip_and_invalid_index() {
        let p = lilka_board_create(std::ptr::null_mut(), no_read, no_write);
        assert!(!p.is_null());
        // SAFETY: This test owns the live bridge; buffers are separate aligned
        // arrays of 64 initialized words and all calls are serialized.
        unsafe {
            for index in 0..2 {
                let mut input =
                    std::array::from_fn::<_, 64, _>(|i| (i as u32).wrapping_mul(0x1020304) ^ index);
                let mut output = [0; 64];
                assert_eq!(lilka_board_spi_sync(p, index, input.as_mut_ptr(), 1), 0);
                assert_eq!(lilka_board_spi_sync(p, index, output.as_mut_ptr(), 0), 0);
                assert_eq!(input, output);
            }
            assert_eq!(lilka_board_spi_sync(p, 2, std::ptr::null_mut(), 0), -1);
            drop(Box::from_raw(p));
        }
    }
    #[test]
    fn ffi_retains_fifo_commands_adc_and_callback_faults() {
        let p = lilka_board_create(std::ptr::null_mut(), no_read, no_write);
        assert!(!p.is_null());
        // SAFETY: This test owns the Box and performs only serialized calls.
        unsafe {
            assert_eq!(lilka_board_audio_irq_pending(p), 0);
            assert_eq!(lilka_board_next_event(p, 0), u64::MAX);
            let b = &mut *p;
            assert!(b.memory.read(0x60024000, 4, Access::Read).is_err());
            assert!(b.memory.write(0x60024000, 4, 1).is_err());
            lilka_board_write(p, 0x60004020, 4, (1 << 7) | (1 << 15) | (1 << 16), 0);
            lilka_board_write(p, 0x60004004, 4, 1 << 16, 0);
            lilka_board_write(p, 0x60024010, 4, 1 << 27, 0);
            lilka_board_write(p, 0x6002401c, 4, 7, 0);
            lilka_board_write(p, 0x60024098, 4, 0x11, 0);
            lilka_board_write(p, 0x60024000, 4, 1 << 24, 0);
            assert_eq!(lilka_board_read(p, 0x60024000, 4, 0), 0);
            assert_eq!((*p).board.state.devices.display_transfers, 1);
            lilka_board_write(p, 0x6000880c, 4, (1 << 17) | (1 << 21), 0);
            assert_ne!(lilka_board_read(p, 0x6000880c, 4, 0) & (1 << 16), 0);
            (*p).inputs.push_back((240, 41, true));
            lilka_board_tick(p, 240);
            assert_eq!(lilka_board_read(p, 0x60004040, 4, 240) & (1 << 9), 0);
            assert!((*p).board.state.errors.is_empty());
            drop(Box::from_raw(p));
        }
    }
}
#[cfg(feature = "wifi-experimental")]
mod wifi;
