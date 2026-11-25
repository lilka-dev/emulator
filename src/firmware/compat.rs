use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};

const KEIRA_APP: &str = "fba05afecadec07a18912d04025515adef1db5b8fc052665a2b67dd40ff2cdbb";
const PATCHES: &[(u32, &[u8], &[u8])] = &[
    (0x4203aa54, &[0x8c, 0xda], &[0xcc, 0xd1]),
    (
        0x420c2890,
        &[0x68, 0x39, 0xca, 0x3f],
        &[0x00, 0xff, 0xde, 0x3f],
    ),
    (0x420c97fd, &[0x66, 0x87, 0x1d], &[0x66, 0xb7, 0x1d]),
    (0x420c99af, &[0x42, 0xa0, 0x3f], &[0x42, 0xa0, 0x0f]),
    (0x420c9a0a, &[0x66, 0x83, 0xd6], &[0x66, 0xd3, 0xd6]),
];

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

#[derive(Default, serde::Serialize)]
pub struct Report {
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
        let result = prepare_app(app, wifi_enabled, KEIRA_APP)?;
        report.keira_known |= result.keira_known;
        report.keira_vfs16 |= result.keira_vfs16;
        report.keira_open_autoconnect |= result.keira_open_autoconnect;
        report.changed |= result.changed;
    }
    Ok(report)
}

fn prepare_app(app: &mut [u8], wifi_enabled: bool, expected: &str) -> Result<Report> {
    let mut report = Report::default();
    let Some(image) = Image::parse(app) else {
        return Ok(report);
    };
    if !image.valid(app) {
        return Ok(report);
    }
    let original = format!("{:x}", Sha256::digest(&app[..image.end])) == expected;
    report.keira_known = original;
    if original && !wifi_enabled {
        return Ok(report);
    }
    let mut candidate = app[..image.end].to_vec();
    let mut offsets = Vec::new();
    let mut recognized = true;
    for &(address, old, new) in PATCHES {
        let Some(at) = image.offset(address, old.len()) else {
            recognized = false;
            break;
        };
        if &candidate[at..at + old.len()] != old && &candidate[at..at + new.len()] != new {
            recognized = false;
            break;
        }
        candidate[at..at + old.len()].copy_from_slice(old);
        offsets.push(at);
    }
    if !recognized {
        return Ok(report);
    }
    if !original {
        image.seal(&mut candidate);
        if format!("{:x}", Sha256::digest(&candidate)) != expected {
            return Ok(report);
        }
    }
    ensure!(
        offsets.len() == PATCHES.len(),
        "Incomplete compatibility signature"
    );
    for (&at, &(_, _, new)) in offsets.iter().zip(PATCHES) {
        candidate[at..at + new.len()].copy_from_slice(new);
    }
    image.seal(&mut candidate);
    report.keira_known = true;
    report.keira_vfs16 = true;
    report.keira_open_autoconnect = true;
    if candidate != app[..image.end] {
        app[..image.end].copy_from_slice(&candidate);
        report.changed = true;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Vec<u8>, String) {
        let mut app = vec![0; 32 + 0xd0000];
        app[0] = 0xe9;
        app[1] = 1;
        app[23] = 1;
        app[24..28].copy_from_slice(&0x42002800u32.to_le_bytes());
        app[28..32].copy_from_slice(&0xd0000u32.to_le_bytes());
        app.resize((app.len() / 16 + 1) * 16 + 32, 0);
        let image = Image::parse(&app).unwrap();
        for &(address, old, _) in PATCHES {
            let at = image.offset(address, old.len()).unwrap();
            app[at..at + old.len()].copy_from_slice(old);
        }
        image.seal(&mut app);
        let digest = format!("{:x}", Sha256::digest(&app));
        (app, digest)
    }
    #[test]
    fn known_application_is_opt_in_and_fix_survives_disabling_wifi() {
        let (mut app, digest) = fixture();
        let source = app.clone();
        assert!(!prepare_app(&mut app, false, &digest).unwrap().changed);
        assert_eq!(app, source);
        let r = prepare_app(&mut app, true, &digest).unwrap();
        assert!(r.changed && r.keira_vfs16);
        assert!(Image::parse(&app).unwrap().valid(&app));
        let fixed = app.clone();
        let r = prepare_app(&mut app, false, &digest).unwrap();
        assert!(r.keira_vfs16 && !r.changed);
        assert_eq!(app, fixed);
    }
    #[test]
    fn changed_application_and_damaged_checksums_are_not_modified() {
        let (app, digest) = fixture();
        for position in [40, app.len() - 1] {
            let mut changed = app.clone();
            changed[position] ^= 1;
            let before = changed.clone();
            assert!(
                !prepare_app(&mut changed, true, &digest)
                    .unwrap()
                    .keira_vfs16
            );
            assert_eq!(changed, before);
        }
        let mut unknown = app;
        assert!(
            !prepare_app(&mut unknown, true, "unknown")
                .unwrap()
                .keira_vfs16
        );
        for length in 0..32 {
            assert!(
                !prepare_app(&mut unknown[..length], true, &digest)
                    .unwrap()
                    .changed
            );
        }
    }
}
