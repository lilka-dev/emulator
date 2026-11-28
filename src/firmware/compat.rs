//! Firmware fixes the emulated board needs, found by code shape instead of by a
//! hash of one build, so every build compiled from the same sources gets them.
//!
//! ESP-IDF keeps at most 8 VFS drivers (`VFS_MAX_COUNT`). KeiraOS already registers
//! 8 (uart, secondary and primary console, stdio, /sd, /spiffs, /tmp, one fd range),
//! so turning Wi-Fi on makes lwIP's socket registration fail with ESP_ERR_NO_MEM and
//! the firmware aborts in a boot loop. The table is widened to 16 entries in RAM the
//! board bridge reserves. Separately, KeiraOS skips auto-connecting to a saved network
//! without a password, which is the only kind the emulator offers.
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, ops::Range};

/// 16 table pointers the bridge zeroes at reset (LILKA_QEMU_KEIRA_VFS16).
const VFS_TABLE: u32 = 0x3fde_ff00;
/// Internal data RAM, where ESP-IDF places `s_vfs_count` and `s_vfs`.
const DRAM: Range<u32> = 0x3fc8_8000..0x3fd0_0000;
const OPEN_NETWORK_SKIP: &[u8] = b"no password found for last SSID, skipping auto connection";

/// `bnei` opcode byte (op0 6, BI0 with m=1); the r nibble of the next byte picks the
/// immediate from B4CONST: 8 -> 8, 0xb -> 16, 0xd -> 64.
const BNEI: u8 = 0x66;
const B4_8: u8 = 0x8;
const B4_16: u8 = 0xb;
const B4_64: u8 = 0xd;

struct Image {
    segments: Vec<(u32, usize, usize)>,
    checksum: usize,
    end: usize,
    hash_appended: bool,
}
impl Image {
    fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 24 || bytes[0] != 0xe9 || !(1..=16).contains(&bytes[1]) {
            return None;
        }
        let mut at: usize = 24;
        let mut segments = Vec::new();
        for _ in 0..bytes[1] {
            let h = bytes.get(at..at.checked_add(8)?)?;
            let address = u32::from_le_bytes(h[..4].try_into().ok()?);
            let length = u32::from_le_bytes(h[4..8].try_into().ok()?) as usize;
            at += 8;
            let end = at.checked_add(length)?;
            bytes.get(at..end)?;
            segments.push((address, at, length));
            at = end;
        }
        let checksum = at.checked_div(16)?.checked_add(1)?.checked_mul(16)? - 1;
        let hash_appended = bytes[23] == 1;
        let end = checksum.checked_add(if hash_appended { 33 } else { 1 })?;
        bytes.get(..end)?;
        Some(Self {
            segments,
            checksum,
            end,
            hash_appended,
        })
    }
    fn offset(&self, address: u32, size: usize) -> Option<usize> {
        self.segments.iter().find_map(|&(base, at, length)| {
            let offset = address.checked_sub(base)? as usize;
            (offset.checked_add(size)? <= length).then_some(at + offset)
        })
    }
    fn address(&self, offset: usize) -> Option<u32> {
        self.segments.iter().find_map(|&(base, at, length)| {
            (at..at + length)
                .contains(&offset)
                .then(|| base + (offset - at) as u32)
        })
    }
    fn word(&self, bytes: &[u8], address: u32) -> Option<u32> {
        let at = self.offset(address, 4)?;
        Some(u32::from_le_bytes(bytes[at..at + 4].try_into().ok()?))
    }
    /// Instruction RAM and flash-mapped code, where literal pools sit beside the code.
    fn executable(&self) -> impl Iterator<Item = (u32, usize, usize)> + '_ {
        self.segments.iter().copied().filter(|&(base, _, _)| {
            (0x4037_0000..0x403e_0000).contains(&base) || (0x4200_0000..0x4400_0000).contains(&base)
        })
    }
    /// Every byte position of executable code: (file offset, address).
    fn code(&self) -> impl Iterator<Item = (usize, u32)> + '_ {
        self.executable().flat_map(|(base, at, length)| {
            (0..length.saturating_sub(2)).map(move |p| (at + p, base + p as u32))
        })
    }
    fn valid(&self, bytes: &[u8]) -> bool {
        let checksum = self
            .segments
            .iter()
            .flat_map(|&(_, at, length)| &bytes[at..at + length])
            .fold(0xef, |a, b| a ^ b);
        bytes[self.checksum] == checksum
            && (!self.hash_appended
                || bytes[self.checksum + 1..self.end]
                    == Sha256::digest(&bytes[..=self.checksum])[..])
    }
    fn seal(&self, bytes: &mut [u8]) {
        bytes[self.checksum] = self
            .segments
            .iter()
            .flat_map(|&(_, at, length)| &bytes[at..at + length])
            .fold(0xef, |a, b| a ^ b);
        if self.hash_appended {
            let digest = Sha256::digest(&bytes[..=self.checksum]);
            bytes[self.checksum + 1..self.end].copy_from_slice(&digest);
        }
    }
}

/// Literal address an `l32r` at `pc` loads, if `code` starts with one.
fn l32r(pc: u32, code: &[u8]) -> Option<u32> {
    (code.len() >= 3 && code[0] & 0x0f == 1).then(|| {
        let imm = u32::from(u16::from_le_bytes([code[1], code[2]]));
        ((pc + 3) & !3).wrapping_sub((0x1_0000 - imm) * 4)
    })
}

fn single<T>(mut found: Vec<T>) -> Option<T> {
    (found.len() == 1).then(|| found.pop()).flatten()
}

/// The ESP-IDF vfs.c instructions that bound the driver table, original or widened.
struct VfsSites {
    /// `esp_vfs_register_common`: bnei index, 8 (16) before growing `s_vfs_count`.
    register_limit: usize,
    /// `esp_vfs_unregister_with_id`: movi bound, 63 (15) checks the id.
    unregister_bound: usize,
    /// The same function's fd-table walk: bnei j, 8 (64) loops back.
    unregister_loop: usize,
    /// Literal-pool words holding `&s_vfs` (or the widened table).
    table_literals: Vec<usize>,
}
impl VfsSites {
    fn find(image: &Image, bytes: &[u8]) -> Option<Self> {
        let loaded = |offset: usize, address: u32| {
            l32r(address, &bytes[offset..]).and_then(|literal| image.word(bytes, literal))
        };
        // Both globals are loaded by consecutive literals just before the limit check.
        let register = single(
            image
                .code()
                .filter(|&(i, _)| bytes[i] == BNEI && matches!(bytes[i + 1] >> 4, B4_8 | B4_16))
                .filter_map(|(i, pc)| {
                    let loads: Vec<u32> = (3..=16usize)
                        .filter(|&k| i >= k)
                        .filter_map(|k| loaded(i - k, pc - k as u32))
                        .collect();
                    let count = loads.iter().copied().find(|count| {
                        DRAM.contains(count)
                            && loads.iter().any(|&t| t == count + 4 || t == VFS_TABLE)
                    })?;
                    Some((i + 1, count + 4))
                })
                .collect(),
        )?;
        let (register_limit, table) = register;
        let is_table = |value: u32| value == table || value == VFS_TABLE;
        let unregister = single(
            image
                .code()
                .filter(|&(i, _)| {
                    bytes[i] & 0x0f == 2
                        && bytes[i + 1] == 0xa0
                        && matches!(bytes[i + 2], 0x3f | 0x0f)
                })
                .filter(|&(i, pc)| {
                    (3..=12).any(|k| loaded(i + k, pc + k as u32).is_some_and(is_table))
                })
                .filter_map(|(i, pc)| {
                    let walk = single(
                        (3..0x100usize)
                            .filter(|&k| i + k + 2 < bytes.len())
                            .filter(|&k| {
                                let b = &bytes[i + k..i + k + 3];
                                let target = (pc + k as u32 + 4).wrapping_add(b[2] as i8 as u32);
                                b[0] == BNEI
                                    && matches!(b[1] >> 4, B4_8 | B4_64)
                                    && b[2] >= 0x80
                                    && target > pc
                            })
                            .collect(),
                    )?;
                    Some((i + 2, i + walk + 1))
                })
                .collect(),
        )?;
        let candidates: HashSet<u32> = image
            .executable()
            .flat_map(|(base, at, length)| {
                ((base.wrapping_neg() % 4) as usize..length.saturating_sub(3))
                    .step_by(4)
                    .map(move |p| (base + p as u32, at + p))
            })
            .filter(|&(_, at)| is_table(u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())))
            .map(|(address, _)| address)
            .collect();
        let used: HashSet<u32> = image
            .code()
            .filter_map(|(i, pc)| l32r(pc, &bytes[i..]))
            .filter(|literal| candidates.contains(literal))
            .collect();
        let mut table_literals: Vec<usize> =
            used.iter().filter_map(|&a| image.offset(a, 4)).collect();
        table_literals.sort_unstable();
        (!table_literals.is_empty()).then_some(Self {
            register_limit,
            unregister_bound: unregister.0,
            unregister_loop: unregister.1,
            table_literals,
        })
    }
    fn widen(&self, bytes: &mut [u8]) {
        bytes[self.register_limit] = B4_16 << 4 | bytes[self.register_limit] & 0x0f;
        bytes[self.unregister_bound] = 0x0f;
        bytes[self.unregister_loop] = B4_64 << 4 | bytes[self.unregister_loop] & 0x0f;
        for &at in &self.table_literals {
            bytes[at..at + 4].copy_from_slice(&VFS_TABLE.to_le_bytes());
        }
    }
    fn relocated(&self, bytes: &[u8]) -> bool {
        self.table_literals
            .iter()
            .any(|&at| bytes[at..at + 4] == VFS_TABLE.to_le_bytes())
    }
}

/// KeiraOS NetworkService: `beqz.n` right before loading the "no password" log line;
/// the fall-through skips auto-connect. Returns the branch offset.
fn find_open_network_skip(image: &Image, bytes: &[u8]) -> Option<usize> {
    let found = bytes
        .windows(OPEN_NETWORK_SKIP.len())
        .position(|w| w == OPEN_NETWORK_SKIP)?;
    let start = bytes[..found]
        .iter()
        .rposition(|&b| b == 0)
        .map_or(0, |z| z + 1);
    let message = image.address(start)?;
    single(
        image
            .code()
            .filter(|&(i, pc)| {
                i >= 2
                    && l32r(pc, &bytes[i..]).is_some_and(|l| image.word(bytes, l) == Some(message))
            })
            .map(|(i, _)| i - 2)
            .filter(|&at| bytes[at] & 0x0f == 0x0c && bytes[at] & 0x80 != 0)
            .filter(|&at| bytes[at] & 0x40 == 0 || bytes[at + 1] & 0x0f == 1)
            .collect(),
    )
}

/// Turns `beqz.n aN, skip` into `bnez.n a1, skip`: a1 is the stack pointer, never zero.
fn open_network(bytes: &mut [u8], at: usize) {
    bytes[at] |= 0x40;
    bytes[at + 1] = bytes[at + 1] & 0xf0 | 1;
}
fn opened(bytes: &[u8], at: usize) -> bool {
    bytes[at] & 0x40 != 0 && bytes[at + 1] & 0x0f == 1
}

#[derive(Default, serde::Serialize)]
pub struct Report {
    /// KeiraOS NetworkService and the VFS fix were both recognized, so its NVS
    /// network settings may be written.
    pub keira_known: bool,
    pub keira_vfs16: bool,
    pub wifi_settings_changed: bool,
    pub keira_open_autoconnect: bool,
    pub changed: bool,
}

pub fn prepare(flash: &mut [u8], wifi_enabled: bool) -> Result<Report> {
    let mut report = Report::default();
    for partition in super::partitions(flash, flash.len())?
        .into_iter()
        .filter(|p| p.kind == 0)
    {
        let app = flash
            .get_mut(partition.offset..partition.offset + partition.size)
            .context("Truncated application partition")?;
        let result = prepare_app(app, wifi_enabled);
        report.keira_known |= result.keira_known;
        report.keira_vfs16 |= result.keira_vfs16;
        report.keira_open_autoconnect |= result.keira_open_autoconnect;
        report.changed |= result.changed;
    }
    Ok(report)
}

fn prepare_app(app: &mut [u8], wifi_enabled: bool) -> Report {
    let mut report = Report::default();
    let Some(image) = Image::parse(app) else {
        return report;
    };
    if !image.valid(app) {
        return report;
    }
    let mut candidate = app[..image.end].to_vec();
    let vfs = VfsSites::find(&image, &candidate);
    let skip = find_open_network_skip(&image, &candidate);
    // A full table aborts whenever Wi-Fi starts, from the profile or the firmware's own menu.
    if let Some(vfs) = &vfs {
        vfs.widen(&mut candidate);
    }
    if let (Some(at), true) = (skip, wifi_enabled && vfs.is_some()) {
        open_network(&mut candidate, at);
    }
    report.keira_known = vfs.is_some() && skip.is_some();
    report.keira_vfs16 = vfs.as_ref().is_some_and(|v| v.relocated(&candidate));
    report.keira_open_autoconnect = skip.is_some_and(|at| opened(&candidate, at));
    if candidate != app[..image.end] {
        image.seal(&mut candidate);
        app[..image.end].copy_from_slice(&candidate);
        report.changed = true;
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODE: u32 = 0x4200_0020;
    const TEXT: u32 = 0x3c00_0020;
    const COUNT: u32 = 0x3fca_3964;

    /// Builds code the way the Xtensa toolchain lays out ESP-IDF vfs.c and KeiraOS.
    struct Asm(Vec<u8>);
    impl Asm {
        fn pc(&self) -> u32 {
            CODE + self.0.len() as u32
        }
        fn literal(&mut self, value: u32) -> u32 {
            while self.pc() % 4 != 0 {
                self.0.push(0);
            }
            let at = self.pc();
            self.0.extend(value.to_le_bytes());
            at
        }
        fn l32r(&mut self, register: u8, literal: u32) {
            let base = (self.pc() + 3) & !3;
            let imm = 0x1_0000 - (base - literal) / 4;
            self.0
                .extend([register << 4 | 1, imm as u8, (imm >> 8) as u8]);
        }
        fn raw(&mut self, bytes: &[u8]) -> usize {
            self.0.extend(bytes);
            self.0.len() - bytes.len()
        }
    }

    struct Sample {
        app: Vec<u8>,
        register: usize,
        bound: usize,
        walk: usize,
        table: usize,
        skip: usize,
    }

    fn sample(with_keira: bool, registers: usize) -> Sample {
        let mut asm = Asm(Vec::new());
        let count = asm.literal(COUNT);
        let table = asm.literal(COUNT + 4);
        let message = asm.literal(TEXT + 1);
        let mut register = 0;
        for _ in 0..registers {
            asm.l32r(9, count);
            asm.l32r(12, table);
            asm.raw(&[0xa8, 0x09, 0x0c, 0x07, 0xa7, 0x97, 0x12]);
            register = asm.raw(&[BNEI, 0x87, 0x1d]) + 1;
        }
        asm.raw(&[0x36, 0x41, 0x00]);
        let movi = asm.pc();
        let bound = asm.raw(&[0x42, 0xa0, 0x3f]) + 2;
        asm.raw(&[0x32, 0xa1, 0x02, 0x27, 0x34, 0x5f]);
        asm.l32r(4, table);
        asm.raw(&[0x0d, 0xf0].repeat(20));
        let back = (movi + 3).wrapping_sub(asm.pc() + 4) as u8;
        let walk = asm.raw(&[BNEI, 0x83, back]) + 1;
        let skip = asm.raw(&[0x8c, 0xda]);
        asm.l32r(11, message);
        asm.raw(&[0x1d, 0xf0]);
        let mut text = vec![0u8];
        if with_keira {
            text.extend(b"NetworkService: ");
            text.extend(OPEN_NETWORK_SKIP);
        }
        text.push(0);
        let mut app = vec![0u8; 24];
        app[0] = 0xe9;
        app[1] = 2;
        app[23] = 1;
        let mut code_at = 0;
        for (address, data) in [(TEXT, &text), (CODE, &asm.0)] {
            app.extend(address.to_le_bytes());
            app.extend((data.len() as u32).to_le_bytes());
            if address == CODE {
                code_at = app.len();
            }
            app.extend(data.iter());
        }
        app.resize((app.len() / 16 + 1) * 16 + 32, 0);
        Image::parse(&app).unwrap().seal(&mut app);
        Sample {
            app,
            register: code_at + register,
            bound: code_at + bound,
            walk: code_at + walk,
            table: code_at + (table - CODE) as usize,
            skip: code_at + skip,
        }
    }

    #[test]
    fn vfs_table_is_widened_and_open_networks_follow_wifi() {
        let Sample {
            mut app,
            register,
            bound,
            walk,
            table,
            skip,
        } = sample(true, 1);
        let r = prepare_app(&mut app, false);
        assert!(r.changed && r.keira_known && r.keira_vfs16 && !r.keira_open_autoconnect);
        assert_eq!(app[register], 0xb7);
        assert_eq!(app[bound], 0x0f);
        assert_eq!(app[walk], 0xd3);
        assert_eq!(app[table..table + 4], VFS_TABLE.to_le_bytes());
        assert_eq!(app[skip..skip + 2], [0x8c, 0xda]);
        assert!(Image::parse(&app).unwrap().valid(&app));

        let r = prepare_app(&mut app, true);
        assert!(r.changed && r.keira_open_autoconnect && r.keira_vfs16);
        assert_eq!(app[skip..skip + 2], [0xcc, 0xd1]);
        assert!(Image::parse(&app).unwrap().valid(&app));

        let fixed = app.clone();
        for wifi in [true, false] {
            let r = prepare_app(&mut app, wifi);
            assert!(!r.changed && r.keira_known && r.keira_vfs16 && r.keira_open_autoconnect);
            assert_eq!(app, fixed);
        }
    }

    #[test]
    fn vfs_fix_applies_without_keira_but_keira_settings_stay_untouched() {
        let Sample { mut app, .. } = sample(false, 1);
        let r = prepare_app(&mut app, true);
        assert!(r.changed && r.keira_vfs16 && !r.keira_known && !r.keira_open_autoconnect);
    }

    #[test]
    fn ambiguous_unknown_and_damaged_images_are_not_modified() {
        let ambiguous = sample(true, 2).app;
        let mut app = ambiguous.clone();
        let r = prepare_app(&mut app, true);
        assert!(!r.changed && !r.keira_known && !r.keira_vfs16);
        assert_eq!(app, ambiguous);

        let app = sample(true, 1).app;
        for position in [40, app.len() - 1] {
            let mut damaged = app.clone();
            damaged[position] ^= 1;
            let before = damaged.clone();
            assert!(!prepare_app(&mut damaged, true).changed);
            assert_eq!(damaged, before);
        }
        for length in 0..32 {
            let mut short = app[..length].to_vec();
            assert!(!prepare_app(&mut short, true).changed);
        }
    }
}
