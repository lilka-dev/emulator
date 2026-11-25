use std::collections::VecDeque;

pub struct OpenAp {
    ssid: Vec<u8>,
    bssid: [u8; 6],
    station: Option<[u8; 6]>,
    sequence: u16,
    next_beacon: u64,
    received: VecDeque<Vec<u8>>,
}

impl OpenAp {
    pub fn new(ssid: &str) -> Result<Self, &'static str> {
        if ssid.is_empty() || ssid.len() > 32 || ssid.contains('\0') {
            return Err("SSID must contain 1–32 bytes");
        }
        Ok(Self {
            ssid: ssid.as_bytes().to_vec(),
            bssid: [2, 0x4c, 0x49, 0x4c, 0x4b, 1],
            station: None,
            sequence: 0,
            next_beacon: 0,
            received: VecDeque::new(),
        })
    }
    fn header(&mut self, control: u16, destination: &[u8; 6], third: &[u8; 6]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(128);
        bytes.extend_from_slice(&control.to_le_bytes());
        bytes.extend_from_slice(&[0, 0]);
        bytes.extend_from_slice(destination);
        bytes.extend_from_slice(&self.bssid);
        bytes.extend_from_slice(third);
        bytes.extend_from_slice(&(self.sequence << 4).to_le_bytes());
        self.sequence = self.sequence.wrapping_add(1) & 0xfff;
        bytes
    }
    fn information(&self, bytes: &mut Vec<u8>) {
        bytes.extend_from_slice(&[0, self.ssid.len() as u8]);
        bytes.extend_from_slice(&self.ssid);
        bytes.extend_from_slice(&[1, 8, 0x82, 0x84, 0x8b, 0x96, 0x0c, 0x12, 0x18, 0x24]);
        bytes.extend_from_slice(&[3, 1, 6]);
    }
    fn announcement(&mut self, control: u16, destination: &[u8; 6], now_us: u64) -> Vec<u8> {
        let bssid = self.bssid;
        let mut frame = self.header(control, destination, &bssid);
        frame.extend_from_slice(&now_us.to_le_bytes());
        frame.extend_from_slice(&100u16.to_le_bytes());
        frame.extend_from_slice(&0x0421u16.to_le_bytes());
        self.information(&mut frame);
        frame
    }
    pub fn step(&mut self, now_us: u64) -> Option<Vec<u8>> {
        if let Some(frame) = self.received.pop_front() {
            return Some(frame);
        }
        if now_us >= self.next_beacon {
            self.next_beacon = now_us.saturating_add(102_400);
            return Some(self.announcement(0x80, &[0xff; 6], now_us));
        }
        None
    }
    pub fn station_tx(&mut self, frame: &[u8], now_us: u64) -> Option<Vec<u8>> {
        if frame.len() < 24 || self.received.len() >= 64 {
            return None;
        }
        let control = u16::from_le_bytes([frame[0], frame[1]]);
        let station: [u8; 6] = frame[10..16].try_into().unwrap();
        match control & 0xfc {
            0x40 => {
                let mut at = 24;
                let mut matches = false;
                while at + 2 <= frame.len() {
                    let size = frame[at + 1] as usize;
                    if at + 2 + size > frame.len() {
                        break;
                    }
                    if frame[at] == 0 {
                        matches = size == 0 || frame[at + 2..at + 2 + size] == self.ssid;
                        break;
                    }
                    at += 2 + size;
                }
                if matches {
                    let reply = self.announcement(0x50, &station, now_us);
                    self.received.push_back(reply);
                }
            }
            0xb0 if frame.len() >= 30 && frame[24..28] == [0, 0, 1, 0] => {
                let bssid = self.bssid;
                let mut reply = self.header(0xb0, &station, &bssid);
                reply.extend_from_slice(&[0, 0, 2, 0, 0, 0]);
                self.received.push_back(reply);
            }
            0x00 => {
                let bssid = self.bssid;
                let mut reply = self.header(0x10, &station, &bssid);
                reply.extend_from_slice(&[0x21, 4, 0, 0, 1, 0xc0]);
                reply.extend_from_slice(&[1, 4, 0x82, 0x84, 0x8b, 0x96]);
                self.station = Some(station);
                self.received.push_back(reply);
            }
            value if value & 0x0c == 8 && control & 0x0300 == 0x0100 => {
                let header = if control & 0x80 != 0 { 26 } else { 24 };
                if self.station != Some(station)
                    || frame.len() < header + 8
                    || frame[header..header + 6] != [0xaa, 0xaa, 3, 0, 0, 0]
                {
                    return None;
                }
                let mut ethernet = Vec::with_capacity(frame.len());
                ethernet.extend_from_slice(&frame[16..22]);
                ethernet.extend_from_slice(&station);
                ethernet.extend_from_slice(&frame[header + 6..]);
                return Some(ethernet);
            }
            _ => {}
        }
        None
    }
    pub fn ethernet_rx(&mut self, ethernet: &[u8]) {
        if ethernet.len() < 14 || ethernet.len() > 1514 || self.received.len() >= 64 {
            return;
        }
        let Some(station) = self.station else {
            return;
        };
        let source: [u8; 6] = ethernet[6..12].try_into().unwrap();
        let mut frame = self.header(0x0208, &station, &source);
        frame.extend_from_slice(&[0xaa, 0xaa, 3, 0, 0, 0]);
        frame.extend_from_slice(&ethernet[12..]);
        self.received.push_back(frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn probe_authentication_and_association_precede_data() {
        let mut ap = OpenAp::new("Lilka").unwrap();
        let station = [2, 3, 4, 5, 6, 7];
        let mut request = vec![0; 24];
        request[0] = 0x40;
        request[10..16].copy_from_slice(&station);
        request.extend_from_slice(&[0, 0]);
        ap.station_tx(&request, 100);
        let response = ap.step(100).unwrap();
        assert_eq!(response[0], 0x50);
        assert_eq!(&response[4..10], &station);
        assert!(response.windows(5).any(|bytes| bytes == b"Lilka"));
        request.truncate(24);
        request[0] = 0xb0;
        request.extend_from_slice(&[0, 0, 1, 0, 0, 0]);
        ap.station_tx(&request, 200);
        assert_eq!(&ap.step(200).unwrap()[24..30], &[0, 0, 2, 0, 0, 0]);
        request[0] = 0;
        ap.station_tx(&request, 300);
        assert_eq!(ap.step(300).unwrap()[0], 0x10);
        let ethernet = [vec![0xff; 6], vec![8; 6], vec![8, 0, 1, 2, 3]].concat();
        ap.ethernet_rx(&ethernet);
        let received = ap.step(400).unwrap();
        assert_eq!(&received[16..22], &[8; 6]);
        assert_eq!(&received[30..], &[8, 0, 1, 2, 3]);
    }
}
