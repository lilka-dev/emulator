pub mod compat;
pub mod nvs;
use anyhow::{Context, Result, ensure};
pub const FLASH_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_FLASH_BYTES: usize = 16 * 1024 * 1024;
#[derive(Debug)]
struct Partition {
    kind: u8,
    subtype: u8,
    offset: usize,
    size: usize,
}
fn word(bytes: &[u8], at: usize) -> Result<usize> {
    Ok(u32::from_le_bytes(
        bytes
            .get(at..at + 4)
            .context("Truncated flash metadata")?
            .try_into()?,
    ) as usize)
}
fn executable(bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() >= 24 && bytes[0] == 0xe9,
        "Expected ESP executable; an app-only .bin needs --app and a bootable base flash"
    );
    ensure!(bytes[12..14] == [9, 0], "Firmware is not for ESP32-S3");
    let mut position = 24;
    for _ in 0..bytes[1] {
        let length = word(bytes, position + 4)?;
        position = position
            .checked_add(8)
            .and_then(|p| p.checked_add(length))
            .context("Image size overflow")?;
        ensure!(position <= bytes.len(), "Truncated executable segment");
    }
    Ok(())
}
fn partitions(bytes: &[u8], capacity: usize) -> Result<Vec<Partition>> {
    let mut result = Vec::new();
    for at in (0x8000..0x8c00).step_by(32) {
        if bytes.get(at..at + 2) != Some(&[0xaa, 0x50]) {
            break;
        }
        let offset = word(bytes, at + 4)?;
        let size = word(bytes, at + 8)?;
        ensure!(
            offset >= 0x9000
                && size > 0
                && offset.checked_add(size).is_some_and(|end| end <= capacity),
            "Partition at {offset:#x} + {size:#x} exceeds supported flash capacity"
        );
        ensure!(
            result
                .iter()
                .all(|p: &Partition| offset >= p.offset + p.size || p.offset >= offset + size),
            "Overlapping partitions"
        );
        result.push(Partition {
            kind: bytes[at + 2],
            subtype: bytes[at + 3],
            offset,
            size,
        });
    }
    Ok(result)
}
pub fn normalize(bytes: &[u8]) -> Result<Vec<u8>> {
    ensure!(bytes.len() <= MAX_FLASH_BYTES, "Image exceeds 16 MiB flash");
    executable(bytes)?;
    let table = partitions(bytes, MAX_FLASH_BYTES)?;
    if table.iter().any(|p| p.kind == 0) {
        ensure!(
            table
                .iter()
                .filter(|p| p.kind == 0)
                .any(|p| bytes.get(p.offset) == Some(&0xe9)),
            "No populated app partition; use --app to install an application"
        );
    }
    let declared = match bytes[3] >> 4 {
        0..=3 => FLASH_BYTES,
        4 => MAX_FLASH_BYTES,
        n => anyhow::bail!("Unsupported ESP flash size code {n}; maximum is 16 MiB"),
    };
    let required = table
        .iter()
        .map(|p| p.offset + p.size)
        .max()
        .unwrap_or(0)
        .max(bytes.len())
        .max(declared);
    ensure!(
        required <= declared,
        "Partition table or image exceeds flash size declared in the firmware header"
    );
    let capacity = declared;
    let mut image = vec![0xff; capacity];
    image[..bytes.len()].copy_from_slice(bytes);
    for part in &table {
        if part.kind == 1
            && part.subtype == 0x82
            && image[part.offset..part.offset + part.size]
                .iter()
                .all(|&byte| byte == 0xff)
            && part.offset.is_multiple_of(4096)
            && part.size.is_multiple_of(4096)
            && (2..=u16::MAX as usize).contains(&(part.size / 4096))
        {
            let blocks = part.size / 4096;
            for block in 0..blocks {
                let position = part.offset + block * 4096 + 252;
                let magic = 0x0429u16 ^ (blocks - block) as u16;
                image[position..position + 2].copy_from_slice(&magic.to_le_bytes());
                image[position + 2..position + 4].copy_from_slice(&0u16.to_le_bytes());
            }
        }
    }
    Ok(image)
}
pub fn install_app(flash: &mut [u8], app: &[u8]) -> Result<()> {
    ensure!(
        [FLASH_BYTES, MAX_FLASH_BYTES].contains(&flash.len()),
        "Expected normalized 8 or 16 MiB flash"
    );
    executable(app)?;
    let entries = partitions(flash, flash.len())?;
    let slot = entries
        .iter()
        .filter(|p| p.kind == 0)
        .min_by_key(|p| match p.subtype {
            0 => 0,
            16 => 1,
            _ => 2,
        })
        .context("Base flash needs a factory/OTA partition table")?;
    ensure!(
        app.len() <= slot.size,
        "Application exceeds the selected partition"
    );
    flash[slot.offset..slot.offset + slot.size].fill(0xff);
    flash[slot.offset..slot.offset + app.len()].copy_from_slice(app);
    for p in entries.iter().filter(|p| p.kind == 1 && p.subtype == 0) {
        flash[p.offset..p.offset + p.size].fill(0xff);
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn header() -> Vec<u8> {
        let mut v = vec![0; 24];
        v[0] = 0xe9;
        v[12] = 9;
        v
    }
    #[test]
    fn short_image_pads_erased_flash_and_rejects_wrong_chip() {
        let mut image = header();
        let flash = normalize(&image).unwrap();
        assert_eq!(flash.len(), FLASH_BYTES);
        assert!(flash[24..].iter().all(|b| *b == 0xff));
        image[12] = 0;
        assert!(normalize(&image).is_err());
    }
    #[test]
    fn provisions_only_missing_spiffs_partition() -> Result<()> {
        let mut image = header();
        image.resize(0x9000, 0xff);
        let entry = &mut image[0x8000..0x8020];
        entry[..4].copy_from_slice(&[0xaa, 0x50, 1, 0x82]);
        entry[4..8].copy_from_slice(&0xc000u32.to_le_bytes());
        entry[8..12].copy_from_slice(&0x4000u32.to_le_bytes());
        let normalized = normalize(&image)?;
        for block in 0..4usize {
            let offset = 0xc000 + block * 4096 + 252;
            assert_eq!(
                &normalized[offset..offset + 2],
                &(0x429u16 ^ (4 - block) as u16).to_le_bytes()
            );
            assert_eq!(&normalized[offset + 2..offset + 4], &[0, 0]);
        }
        let mut erased_dump = image.clone();
        erased_dump.resize(FLASH_BYTES, 0xff);
        let reinitialized = normalize(&erased_dump)?;
        assert_eq!(reinitialized[0xc0fc..0xc100], normalized[0xc0fc..0xc100]);
        image.resize(0xc001, 0xff);
        image[0xc000] = 0x42;
        let existing = normalize(&image)?;
        assert_eq!(existing[0xc000], 0x42);
        assert_eq!(&existing[0xc0fc..0xc100], &[0xff; 4]);
        Ok(())
    }
    #[test]
    fn declared_flash_capacity_selects_sixteen_mib_without_changing_bytes() -> Result<()> {
        let mut image = header();
        image[3] = 0x40;
        let normalized = normalize(&image)?;
        assert_eq!(normalized.len(), MAX_FLASH_BYTES);
        assert_eq!(&normalized[..image.len()], image);
        assert!(normalized[image.len()..].iter().all(|b| *b == 0xff));
        image[3] = 0x50;
        assert!(normalize(&image).is_err());
        Ok(())
    }
    #[test]
    fn selects_larger_flash_and_rejects_out_of_bounds_partition() -> Result<()> {
        let mut image = header();
        image.resize(0x9000, 0xff);
        let e = &mut image[0x8000..0x8020];
        e[..2].copy_from_slice(&[0xaa, 0x50]);
        e[2] = 1;
        e[4..8].copy_from_slice(&0x790000u32.to_le_bytes());
        e[8..12].copy_from_slice(&0x100000u32.to_le_bytes());
        assert!(normalize(&image).is_err());
        image[3] = 0x40;
        assert_eq!(normalize(&image)?.len(), MAX_FLASH_BYTES);
        let e = &mut image[0x8000..0x8020];
        e[4..8].copy_from_slice(&0xff0000u32.to_le_bytes());
        e[8..12].copy_from_slice(&0x20000u32.to_le_bytes());
        assert!(normalize(&image).is_err());
        Ok(())
    }
    #[test]
    fn app_install_erases_only_slot_and_ota_selection() {
        let mut flash = header();
        flash.resize(FLASH_BYTES, 0xff);
        for (i, kind, sub, offset, size) in [
            (0, 0, 16, 0x10000u32, 0x100000u32),
            (1, 1, 0, 0xe000, 0x2000),
        ] {
            let e = &mut flash[0x8000 + i * 32..0x8020 + i * 32];
            e[..2].copy_from_slice(&[0xaa, 0x50]);
            e[2] = kind;
            e[3] = sub;
            e[4..8].copy_from_slice(&offset.to_le_bytes());
            e[8..12].copy_from_slice(&size.to_le_bytes());
        }
        flash[0xe000] = 1;
        flash[0x9000] = 42;
        let app = header();
        install_app(&mut flash, &app).unwrap();
        assert_eq!(&flash[0x10000..0x10018], app);
        assert_eq!(flash[0xe000], 0xff);
        assert_eq!(flash[0x9000], 42);
    }
}
