mod ap;
mod mac;

use lilka_board::bus::{Access, Bus};
use std::collections::BTreeMap;

pub struct Radio {
    ssid: String,
    mac: mac::WifiMac,
    ap: ap::OpenAp,
    rx: std::collections::VecDeque<Vec<u8>>,
    network: crate::net::NetworkWorker,
    pub transmitted: u64,
    station_ipv4: Option<[u8; 4]>,
    pub received: u64,
    pub dropped: u64,
    pub last_read: (u32, u32),
    pub last_write: (u32, u32),
    last_rx: u32,
}

fn ram(address: u32, length: usize) -> bool {
    address >= 0x3fc8_0000
        && address
            .checked_add(length as u32)
            .is_some_and(|end| end <= 0x3fd0_0000)
}
fn word(bus: &mut impl Bus, address: u32) -> Option<u32> {
    if !ram(address, 4) || address & 3 != 0 {
        return None;
    }
    bus.read(address, 4, Access::Read).ok()
}
fn checksum(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320u32.wrapping_mul(crc & 1));
        }
    }
    !crc
}

impl Radio {
    pub fn new(ssid: &str) -> Result<Self, &'static str> {
        Ok(Self {
            ssid: ssid.into(),
            mac: mac::WifiMac::new(240_000_000),
            ap: ap::OpenAp::new(ssid)?,
            rx: Default::default(),
            network: crate::net::NetworkWorker::new(),
            transmitted: 0,
            station_ipv4: None,
            received: 0,
            dropped: 0,
            last_read: (0, 0),
            last_write: (0, 0),
            last_rx: 0,
        })
    }
    pub fn diagnostics(&self, bus: &mut impl Bus) -> serde_json::Value {
        let descriptor = self.mac.rx_next | 0x3fc0_0000;
        serde_json::json!({
            "tx_frames": self.transmitted, "rx_frames": self.received,
            "dropped": self.dropped, "last_read": self.last_read, "last_write": self.last_write,
            "rx_base": self.mac.rx_base, "rx_next": self.mac.rx_next,
            "last_rx": self.last_rx, "last_rx_flags": word(bus, self.last_rx),
            "next_flags": word(bus, descriptor), "next_buffer": word(bus, descriptor + 4),
            "next_link": word(bus, descriptor + 8), "queued": self.rx.len(), "station_ipv4": self.station_ipv4
        })
    }
    pub fn irq(&self) -> bool {
        self.mac.irq()
    }
    pub fn handles(address: u32) -> bool {
        matches!(address, 0x6003_3000..=0x6003_4fff | 0x6003_500c..=0x6003_501f |
            0x6003_5118..=0x6003_511f | 0x6003_5128..=0x6003_512b)
    }
    pub fn reset(&mut self) {
        self.mac = mac::WifiMac::new(240_000_000);
        self.ap = ap::OpenAp::new(&self.ssid).unwrap();
        self.rx.clear();
        self.network = crate::net::NetworkWorker::new();
        self.last_rx = 0;
        self.station_ipv4 = None;
    }
    pub fn read(&mut self, address: u32, now: u64) -> u32 {
        self.mac.now_cycles = now;
        let value = self
            .mac
            .read((address - 0x6000_0000) >> 12, address & 0xfff);
        self.last_read = (address, value);
        value
    }
    pub fn write(&mut self, address: u32, value: u32, now: u64) {
        self.mac.now_cycles = now;
        self.last_write = (address, value);
        self.mac
            .write((address - 0x6000_0000) >> 12, address & 0xfff, value);
    }

    fn armed(
        &self,
        bus: &mut impl Bus,
        descriptor: u32,
        length: usize,
    ) -> Option<(u32, u32, u32, u32)> {
        if descriptor == 0 {
            return None;
        }
        let descriptor = descriptor | 0x3fc0_0000;
        let flags = word(bus, descriptor)?;
        let buffer = word(bus, descriptor + 4)?;
        let next = word(bus, descriptor + 8)?;
        (flags & (1 << 31) != 0
            && flags & (1 << 30) == 0
            && (flags & 0xfff) as usize >= length
            && ram(buffer, length))
        .then_some((descriptor, flags, buffer, next))
    }
    pub fn advance(&mut self, now: u64, bus: &mut impl Bus) {
        let now_us = now / 240;
        for (queue, descriptor) in std::mem::take(&mut self.mac.tx_pending) {
            if let (Some(flags), Some(buffer)) = (word(bus, descriptor), word(bus, descriptor + 4))
            {
                let length = ((flags >> 12) & 0xfff) as usize;
                if length >= 24 && ram(buffer, length) {
                    let mut frame = vec![0; length];
                    if bus.read_bytes(buffer, &mut frame).is_ok() {
                        self.transmitted += 1;
                        if let Some(ethernet) = self.ap.station_tx(&frame, now_us) {
                            if ethernet.len() >= 34
                                && ethernet[12..14] == [8, 0]
                                && ethernet[14] >> 4 == 4
                                && ethernet[26..30] != [0; 4]
                            {
                                self.station_ipv4 = Some(ethernet[26..30].try_into().unwrap());
                            }
                            if !self.network.send(ethernet) {
                                self.dropped += 1;
                            }
                        }
                    }
                }
            }
            self.mac.tx_done(queue);
        }
        for ethernet in self.network.receive() {
            self.ap.ethernet_rx(&ethernet);
        }
        if let Some(frame) = self.ap.step(now_us)
            && self.mac.rx_base != 0
        {
            if self.rx.len() < 64 {
                self.rx.push_back(frame);
            } else {
                self.dropped += 1;
            }
        }
        let Some(frame) = self.rx.front() else {
            return;
        };
        let total = 48 + frame.len() + 4;
        let mut armed = self.armed(bus, self.mac.rx_next, total);
        if armed.is_none() && self.mac.rx_restart_at_base() {
            armed = self.armed(bus, self.mac.rx_next, total);
        }
        let Some((descriptor, flags, buffer, next)) = armed else {
            return;
        };
        let frame = self.rx.pop_front().unwrap();
        let mut bytes = Vec::with_capacity(total);
        let filter = if frame[4] & 1 != 0 {
            1 << 28
        } else {
            (1 << 28) | (1 << 29)
        };
        for value in [
            filter | u32::from((-40i8) as u8),
            0,
            (6 << 16) | (6 << 20),
            now_us as u32,
            0,
            0xa6,
            0,
            0,
            0,
            0,
            0,
            (frame.len() + 4) as u32,
        ] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(&frame);
        bytes.extend_from_slice(&checksum(&frame).to_le_bytes());
        if bus.write_bytes(buffer, &bytes).is_err() {
            self.dropped += 1;
            return;
        }
        let complete = (flags & !(0xfff << 12)) | ((total as u32) << 12) | (1 << 30) | (1 << 31);
        if bus.write(descriptor, 4, complete).is_err() {
            self.dropped += 1;
            return;
        }
        self.mac.rx_filled(descriptor, next);
        self.last_rx = descriptor;
        self.received += 1;
    }
}

#[derive(Default)]
struct RegisterBank(BTreeMap<u32, u32>);
impl RegisterBank {
    fn new() -> Self {
        Self::default()
    }
    fn read(&self, address: u32) -> u32 {
        self.0.get(&address).copied().unwrap_or(0)
    }
    fn write(&mut self, address: u32, value: u32) {
        self.0.insert(address, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const BASE: u32 = 0x3fca_0000;
    struct Ram(Vec<u8>);
    impl Bus for Ram {
        fn read(
            &mut self,
            address: u32,
            width: u8,
            access: Access,
        ) -> Result<u32, lilka_board::bus::MemoryFault> {
            let offset = address.checked_sub(BASE).unwrap_or(u32::MAX) as usize;
            let source = self
                .0
                .get(offset..offset.saturating_add(width as usize))
                .filter(|_| matches!(width, 1 | 2 | 4))
                .ok_or(lilka_board::bus::MemoryFault { address, access })?;
            let mut bytes = [0; 4];
            bytes[..width as usize].copy_from_slice(source);
            Ok(u32::from_le_bytes(bytes))
        }
        fn write(
            &mut self,
            address: u32,
            width: u8,
            value: u32,
        ) -> Result<(), lilka_board::bus::MemoryFault> {
            let offset = address.checked_sub(BASE).unwrap_or(u32::MAX) as usize;
            let target = self
                .0
                .get_mut(offset..offset.saturating_add(width as usize))
                .filter(|_| matches!(width, 1 | 2 | 4))
                .ok_or(lilka_board::bus::MemoryFault {
                    address,
                    access: Access::Write,
                })?;
            target.copy_from_slice(&value.to_le_bytes()[..width as usize]);
            Ok(())
        }
    }
    #[test]
    fn receive_dma_sets_length_crc_and_ownership_without_overwriting_a_busy_descriptor() {
        let mut memory = Ram(vec![0xa5; 65536]);
        let descriptor = BASE + 0x100;
        let buffer = BASE + 0x1000;
        memory.write(descriptor, 4, (1 << 31) | 2048).unwrap();
        memory.write(descriptor + 4, 4, buffer).unwrap();
        memory.write(descriptor + 8, 4, 0).unwrap();
        let mut radio = Radio::new("Fixture").unwrap();
        radio.write(0x6003_3088, descriptor & 0xfffff, 0);
        radio.advance(0, &mut memory);
        assert_eq!(radio.received, 1);
        assert!(radio.irq());
        let flags = memory.read(descriptor, 4, Access::Read).unwrap();
        assert_eq!(flags & (3 << 30), 3 << 30);
        let length = ((flags >> 12) & 0xfff) as usize;
        let packet = &memory.0[0x1000..0x1000 + length];
        assert_eq!(packet[48], 0x80);
        assert_eq!(
            u32::from_le_bytes(packet[length - 4..].try_into().unwrap()),
            checksum(&packet[48..length - 4])
        );
        assert_eq!(memory.0[0x1000 + length], 0xa5);
        radio.advance(48_000_000, &mut memory);
        assert_eq!(radio.received, 1);
        memory.write(descriptor, 4, (1 << 31) | 2048).unwrap();
        radio.write(0x6003_3084, 1, 48_000_000);
        radio.advance(48_000_240, &mut memory);
        assert_eq!(radio.received, 2);
        assert_eq!(checksum(b"123456789"), 0xcbf4_3926);
    }
    #[test]
    fn receive_ring_can_fill_next_descriptor_before_software_recycles_previous() {
        let mut memory = Ram(vec![0; 65536]);
        let first = BASE + 0x100;
        let second = first + 12;
        for (descriptor, buffer, next) in
            [(first, BASE + 0x1000, second), (second, BASE + 0x2000, 0)]
        {
            memory.write(descriptor, 4, (1 << 31) | 2048).unwrap();
            memory.write(descriptor + 4, 4, buffer).unwrap();
            memory.write(descriptor + 8, 4, next).unwrap();
        }
        let mut radio = Radio::new("Fixture").unwrap();
        radio.write(0x6003_3088, first & 0xfffff, 0);
        radio.advance(0, &mut memory);
        let first_flags = word(&mut memory, first).unwrap();
        radio.advance(48_000_000, &mut memory);
        assert_eq!(radio.received, 2);
        assert_eq!(word(&mut memory, first), Some(first_flags));
        assert_eq!(word(&mut memory, second).unwrap() & (3 << 30), 3 << 30);
    }
    #[test]
    fn rx_handover_preserves_a_running_ring_and_acknowledges_interrupts() {
        let mut mac = mac::WifiMac::new(240_000_000);
        mac.write(0x33, 0x88, 0x10000);
        assert_eq!(mac.rx_next, 0x3fc10000);
        mac.rx_filled(0x3fc10000, 0x3fc1000c);
        assert!(mac.irq());
        mac.write(0x33, 0x88, 0x20000);
        assert_eq!(mac.rx_next, 0x1000c);
        mac.write(0x33, 0xc40, u32::MAX);
        assert!(!mac.irq());
        assert_eq!(mac.read(0x33, 0xd14) & 1, 1);
    }
}
