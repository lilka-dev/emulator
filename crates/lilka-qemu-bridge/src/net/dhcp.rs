const SERVER: [u8; 4] = [192, 168, 4, 1];
const CLIENT: [u8; 4] = [192, 168, 4, 2];
const MAC: [u8; 6] = [2, 0x4c, 0x49, 0x4c, 0x4b, 1];
fn checksum(bytes: &[u8]) -> u16 {
    let mut sum: u32 = bytes
        .chunks(2)
        .map(|c| u32::from(c[0]) * 256 + u32::from(*c.get(1).unwrap_or(&0)))
        .sum();
    while sum > 65535 {
        sum = (sum & 65535) + (sum >> 16);
    }
    !(sum as u16)
}
pub fn reply(frame: &[u8]) -> Option<Vec<u8>> {
    if frame.len() < 42 || frame[12..14] != [8, 0] {
        return None;
    }
    let ip = &frame[14..];
    let header = usize::from(ip[0] & 15) * 4;
    let length = usize::from(u16::from_be_bytes([ip[2], ip[3]]));
    if ip[0] >> 4 != 4
        || header < 20
        || length > ip.len()
        || length < header + 8
        || ip[9] != 17
        || ip[6] & 0x3f != 0
        || ip[7] != 0
        || checksum(&ip[..header]) != 0
    {
        return None;
    }
    let udp = &ip[header..length];
    let udp_length = usize::from(u16::from_be_bytes([udp[4], udp[5]]));
    if udp[..4] != [0, 68, 0, 67] || udp_length > udp.len() || udp_length < 248 {
        return None;
    }
    let request = &udp[8..udp_length];
    if request[..3] != [1, 1, 6] || request[236..240] != [99, 130, 83, 99] {
        return None;
    }
    let mut kind = 0;
    let mut requested = None;
    let mut server = None;
    let mut options = &request[240..];
    while let Some((&tag, rest)) = options.split_first() {
        options = rest;
        if tag == 255 {
            break;
        }
        if tag == 0 {
            continue;
        }
        let (&length, rest) = options.split_first()?;
        let n = usize::from(length);
        if rest.len() < n {
            return None;
        }
        let value = &rest[..n];
        options = &rest[n..];
        match (tag, n) {
            (53, 1) => kind = value[0],
            (50, 4) => requested = Some(value),
            (54, 4) => server = Some(value),
            _ => {}
        }
    }
    if !matches!(kind, 1 | 3) || server.is_some_and(|s| s != SERVER) {
        return None;
    }
    if kind == 3
        && (requested.is_some_and(|p| p != CLIENT)
            || (requested.is_none() && request[12..16] != CLIENT))
    {
        return None;
    }
    let mut bootp = vec![0; 240];
    bootp[..4].copy_from_slice(&[2, 1, 6, 0]);
    bootp[4..12].copy_from_slice(&request[4..12]);
    bootp[16..20].copy_from_slice(&CLIENT);
    bootp[20..24].copy_from_slice(&SERVER);
    bootp[28..44].copy_from_slice(&request[28..44]);
    bootp[236..240].copy_from_slice(&[99, 130, 83, 99]);
    bootp.extend_from_slice(&[53, 1, if kind == 1 { 2 } else { 5 }, 54, 4]);
    bootp.extend_from_slice(&SERVER);
    bootp.extend_from_slice(&[51, 4, 0, 1, 81, 128, 1, 4, 255, 255, 255, 0, 3, 4]);
    bootp.extend_from_slice(&SERVER);
    bootp.extend_from_slice(&[6, 4]);
    bootp.extend_from_slice(&SERVER);
    bootp.push(255);
    bootp.resize(bootp.len().max(300), 0);
    let mut out = vec![0; 42];
    out[..6].fill(255);
    out[6..12].copy_from_slice(&MAC);
    out[12..14].copy_from_slice(&[8, 0]);
    out[14] = 0x45;
    out[16..18].copy_from_slice(&((28 + bootp.len()) as u16).to_be_bytes());
    out[22] = 64;
    out[23] = 17;
    out[26..30].copy_from_slice(&SERVER);
    out[30..34].fill(255);
    let check = checksum(&out[14..34]);
    out[24..26].copy_from_slice(&check.to_be_bytes());
    out[34..38].copy_from_slice(&[0, 67, 0, 68]);
    out[38..40].copy_from_slice(&((8 + bootp.len()) as u16).to_be_bytes());
    out.extend_from_slice(&bootp);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(kind: u8) -> Vec<u8> {
        let mut f = vec![0; 42 + 244];
        f[12..14].copy_from_slice(&[8, 0]);
        f[14] = 0x45;
        f[16..18].copy_from_slice(&272u16.to_be_bytes());
        f[23] = 17;
        let c = checksum(&f[14..34]);
        f[24..26].copy_from_slice(&c.to_be_bytes());
        f[34..38].copy_from_slice(&[0, 68, 0, 67]);
        f[38..40].copy_from_slice(&252u16.to_be_bytes());
        f[42..45].copy_from_slice(&[1, 1, 6]);
        f[46..50].copy_from_slice(&[1, 2, 3, 4]);
        f[54..58].copy_from_slice(&CLIENT);
        f[70..76].copy_from_slice(&[2, 3, 4, 5, 6, 7]);
        f[278..286].copy_from_slice(&[99, 130, 83, 99, 53, 1, kind, 255]);
        f
    }
    #[test]
    fn discover_request_and_truncation() {
        for (kind, expected) in [(1, 2), (3, 5)] {
            let f = request(kind);
            let r = reply(&f).unwrap();
            assert_eq!(&r[46..50], &[1, 2, 3, 4]);
            assert_eq!(&r[58..62], &CLIENT);
            assert_eq!(r[284], expected);
            assert_eq!(checksum(&r[14..34]), 0);
            for n in 0..f.len() {
                assert!(reply(&f[..n]).is_none());
            }
        }
    }
}
