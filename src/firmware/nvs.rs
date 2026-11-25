use anyhow::{Result, bail, ensure};
const PAGE: usize = 4096;
const COUNT: usize = 126;
fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}
fn crc(bytes: impl IntoIterator<Item = u8>) -> u32 {
    let mut value = 0u32;
    for byte in bytes {
        value ^= u32::from(byte);
        for _ in 0..8 {
            value = (value >> 1) ^ (0xedb88320u32.wrapping_mul(value & 1));
        }
    }
    !value
}
fn entry_crc(bytes: &[u8]) -> u32 {
    crc(bytes[..4].iter().chain(&bytes[8..32]).copied())
}
fn state(page: &[u8], index: usize) -> u8 {
    (page[32 + index / 4] >> ((index % 4) * 2)) & 3
}
fn set_state(page: &mut [u8], index: usize, value: u8) {
    let shift = (index % 4) * 2;
    let at = 32 + index / 4;
    page[at] = (page[at] & !(3 << shift)) | (value << shift);
}
#[derive(Clone)]
struct Record {
    page: usize,
    index: usize,
    span: usize,
    namespace: u8,
    kind: u8,
    key: Vec<u8>,
    canonical_key: bool,
    data: Vec<u8>,
}
fn records(bytes: &[u8]) -> Result<(Vec<Record>, Vec<(u32, usize)>)> {
    ensure!(
        bytes.len() >= 2 * PAGE && bytes.len() % PAGE == 0,
        "Unsupported NVS partition size"
    );
    let mut pages = Vec::new();
    for (index, page) in bytes.chunks_exact(PAGE).enumerate() {
        if page.iter().all(|b| *b == 255) {
            continue;
        }
        ensure!(
            matches!(u32_at(page, 0), 0xfffffffe | 0xfffffffc)
                && page[8] == 0xfe
                && u32_at(page, 28) == crc(page[4..28].iter().copied()),
            "Unsupported, encrypted or damaged NVS page {index}"
        );
        pages.push((u32_at(page, 4), index));
    }
    pages.sort_unstable();
    let mut result = Vec::new();
    for &(_, number) in &pages {
        let page = &bytes[number * PAGE..(number + 1) * PAGE];
        let mut index = 0;
        while index < COUNT {
            let status = state(page, index);
            if status == 3 {
                index += 1;
                continue;
            }
            let e = &page[64 + index * 32..96 + index * 32];
            let span = usize::from(e[2]);
            let valid = span > 0 && index + span <= COUNT && u32_at(e, 4) == entry_crc(e);
            if status != 2 {
                index += if valid { span } else { 1 };
                continue;
            }
            ensure!(valid, "Damaged NVS entry {number}:{index}");
            let end = e[8..24].iter().position(|b| *b == 0).unwrap_or(16);
            let data = if e[1] == 0x21 {
                let n = usize::from(u16::from_le_bytes([e[24], e[25]]));
                ensure!(n > 0 && n <= (span - 1) * 32, "Damaged NVS string length");
                let data = page[96 + index * 32..96 + index * 32 + n].to_vec();
                ensure!(
                    data.last() == Some(&0) && u32_at(e, 28) == crc(data.iter().copied()),
                    "Damaged NVS string"
                );
                data
            } else {
                e[24..32].to_vec()
            };
            result.push(Record {
                page: number,
                index,
                span,
                namespace: e[0],
                kind: e[1],
                key: e[8..8 + end].to_vec(),
                canonical_key: end < 16 && e[8 + end..24].iter().all(|b| *b == 0),
                data,
            });
            index += span;
        }
    }
    Ok((result, pages))
}
fn append(bytes: &mut [u8], namespace: u8, key: &[u8], kind: u8, data: &[u8]) -> Result<()> {
    let (old, pages) = records(bytes)?;
    let span = if kind == 0x21 {
        1 + data.len().div_ceil(32)
    } else {
        1
    };
    let mut destination = None;
    for &(_, number) in pages.iter().rev() {
        let page = &bytes[number * PAGE..(number + 1) * PAGE];
        if u32_at(page, 0) != 0xfffffffe {
            continue;
        }
        let last = (0..COUNT)
            .rev()
            .find(|&i| state(page, i) != 3)
            .map_or(0, |i| i + 1);
        if last + span <= COUNT {
            destination = Some((number, last));
            break;
        }
    }
    if destination.is_none() {
        let empty: Vec<_> = bytes
            .chunks_exact(PAGE)
            .enumerate()
            .filter(|(_, p)| p.iter().all(|b| *b == 255))
            .map(|(i, _)| i)
            .collect();
        ensure!(
            empty.len() >= 2,
            "NVS has insufficient free space; no existing settings were erased"
        );
        let number = empty[0];
        let seq = pages
            .last()
            .map_or(Some(0), |(s, _)| s.checked_add(1))
            .ok_or_else(|| anyhow::anyhow!("NVS sequence exhausted"))?;
        for &(_, i) in &pages {
            if u32_at(bytes, i * PAGE) == 0xfffffffe {
                bytes[i * PAGE..i * PAGE + 4].copy_from_slice(&0xfffffffcu32.to_le_bytes());
            }
        }
        let page = &mut bytes[number * PAGE..(number + 1) * PAGE];
        page[..4].copy_from_slice(&0xfffffffeu32.to_le_bytes());
        page[4..8].copy_from_slice(&seq.to_le_bytes());
        page[8] = 0xfe;
        let check = crc(page[4..28].iter().copied());
        page[28..32].copy_from_slice(&check.to_le_bytes());
        destination = Some((number, 0));
    }
    let (number, index) = destination.unwrap();
    let mut header = [255u8; 32];
    header[0] = namespace;
    header[1] = kind;
    header[2] = span as u8;
    header[8..24].fill(0);
    header[8..8 + key.len()].copy_from_slice(key);
    header[8 + key.len()] = 0;
    if kind == 0x21 {
        header[24..26].copy_from_slice(&(data.len() as u16).to_le_bytes());
        header[28..32].copy_from_slice(&crc(data.iter().copied()).to_le_bytes());
    } else {
        header[24..24 + data.len()].copy_from_slice(data);
    }
    let check = entry_crc(&header);
    header[4..8].copy_from_slice(&check.to_le_bytes());
    let page = &mut bytes[number * PAGE..(number + 1) * PAGE];
    let at = 64 + index * 32;
    ensure!(
        page[at..at + span * 32].iter().all(|b| *b == 255),
        "NVS free bitmap conflicts with stored bytes"
    );
    page[at..at + 32].copy_from_slice(&header);
    if kind == 0x21 {
        page[at + 32..at + 32 + data.len()].copy_from_slice(data);
    }
    for i in index..index + span {
        set_state(page, i, 2);
    }
    for r in old
        .iter()
        .filter(|r| r.namespace == namespace && r.key == key)
    {
        let page = &mut bytes[r.page * PAGE..(r.page + 1) * PAGE];
        for i in r.index..r.index + r.span {
            set_state(page, i, 0);
        }
    }
    Ok(())
}
fn upsert(bytes: &mut [u8], namespace: u8, key: &[u8], kind: u8, data: &[u8]) -> Result<bool> {
    let (entries, _) = records(bytes)?;
    if entries
        .iter()
        .rev()
        .find(|r| r.namespace == namespace && r.key == key)
        .is_some_and(|r| {
            r.kind == kind
                && r.canonical_key
                && if kind == 0x21 {
                    r.data == data
                } else {
                    r.data.starts_with(data)
                }
        })
    {
        return Ok(false);
    }
    append(bytes, namespace, key, kind, data)?;
    Ok(true)
}
pub fn configure_wifi(flash: &mut [u8], ssid: Option<&str>) -> Result<bool> {
    let Some(part) = super::partitions(flash, flash.len())?
        .into_iter()
        .find(|p| p.kind == 1 && p.subtype == 2)
    else {
        bail!("Keira firmware has no NVS partition")
    };
    let mut bytes = flash[part.offset..part.offset + part.size].to_vec();
    let (entries, _) = records(&bytes)?;
    let mut changed = false;
    let namespace = if let Some(r) = entries
        .iter()
        .rev()
        .find(|r| r.namespace == 0 && r.kind == 1 && r.key == b"network")
    {
        r.data[0]
    } else {
        if ssid.is_none() {
            return Ok(false);
        }
        let id = entries
            .iter()
            .filter(|r| r.namespace == 0 && r.kind == 1)
            .map(|r| r.data[0])
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .filter(|i| *i < 255)
            .ok_or_else(|| anyhow::anyhow!("NVS namespace limit reached"))?;
        append(&mut bytes, 0, b"network", 1, &[id])?;
        changed = true;
        id
    };
    changed |= upsert(&mut bytes, 0, b"network", 1, &[namespace])?;
    changed |= upsert(
        &mut bytes,
        namespace,
        b"enabled",
        1,
        &[u8::from(ssid.is_some())],
    )?;
    if let Some(ssid) = ssid {
        ensure!(
            (1..=32).contains(&ssid.len()) && !ssid.contains('\0'),
            "Invalid Wi-Fi SSID"
        );
        let mut data = ssid.as_bytes().to_vec();
        data.push(0);
        changed |= upsert(&mut bytes, namespace, b"last_ssid", 0x21, &data)?;
    }
    if changed {
        flash[part.offset..part.offset + part.size].copy_from_slice(&bytes);
    }
    Ok(changed)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn updates_preserve_unrelated_values_and_are_idempotent() {
        let mut p = vec![255; 5 * PAGE];
        append(&mut p, 0, b"misc", 1, &[1]).unwrap();
        append(&mut p, 1, b"volume", 1, &[7]).unwrap();
        append(&mut p, 0, b"network", 1, &[2]).unwrap();
        assert!(upsert(&mut p, 2, b"last_ssid", 0x21, b"Lilka\0").unwrap());
        let before = p.clone();
        assert!(!upsert(&mut p, 2, b"last_ssid", 0x21, b"Lilka\0").unwrap());
        assert_eq!(p, before);
        upsert(&mut p, 2, b"last_ssid", 0x21, "Тест\0".as_bytes()).unwrap();
        let (r, _) = records(&p).unwrap();
        assert!(r.iter().any(|r| r.key == b"volume" && r.data[0] == 7));
        assert_eq!(r.iter().filter(|r| r.key == b"last_ssid").count(), 1);
    }
    #[test]
    fn crc_matches_sdk_known_page_header() {
        let mut h = [255; 32];
        h[..4].copy_from_slice(&0xfffffffcu32.to_le_bytes());
        h[4..8].fill(0);
        h[8] = 0xfe;
        assert_eq!(crc(h[4..28].iter().copied()), 0xb9ba2d84);
    }
}

#[cfg(test)]
mod sdk_encoding_test {
    #[test]
    fn namespace_entry_matches_real_sdk_bytes() {
        let mut page = vec![255; 5 * super::PAGE];
        super::append(&mut page, 0, b"network", 1, &[2]).unwrap();
        assert_eq!(super::u32_at(&page, 64 + 4), 0xdf69d3f3);
        assert_eq!(&page[64 + 8..64 + 24], b"network\0\0\0\0\0\0\0\0\0");
    }
}
