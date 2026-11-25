use super::RegisterBank;
const DMA_ADDR_BASE: u32 = 0x3fc0_0000;
pub const TXQ_COUNT: u32 = 11;

pub struct WifiMac {
    pub ram: RegisterBank,
    pub ram2: RegisterBank,
    pub log: bool,
    pub tsf_offset: i64,
    pub tsf_latched: u64,
    pub now_cycles: u64,
    cycles_per_us: u64,
    pub events: u32,
    pub pwr_events: u32,
    pub txq_complete: u32,
    pub txq_error: u32,
    pub tx_pending: Vec<(u8, u32)>,
    pub rx_base: u32,
    pub rx_next: u32,
    pub rx_last: u32,
    pub rx_handover: bool,
}

impl WifiMac {
    pub fn new(cpu_hz: u64) -> Self {
        WifiMac {
            ram: RegisterBank::new(),
            ram2: RegisterBank::new(),
            log: false,
            tsf_offset: 0,
            tsf_latched: 0,
            now_cycles: 0,
            cycles_per_us: cpu_hz / 1_000_000,
            events: 0,
            pwr_events: 0,
            txq_complete: 0,
            txq_error: 0,
            tx_pending: Vec::new(),
            rx_base: 0,
            rx_next: 0,
            rx_last: 0,
            rx_handover: false,
        }
    }
    pub fn irq(&self) -> bool {
        self.events != 0 || self.pwr_events != 0
    }
    fn txq_of(off: u32) -> Option<u8> {
        if off <= 0xd08 && (0xd08 - off).is_multiple_of(8) && (0xd08 - off) / 8 < TXQ_COUNT {
            Some(((0xd08 - off) / 8) as u8)
        } else {
            None
        }
    }
    pub fn read(&mut self, block: u32, off: u32) -> u32 {
        let v = match (block, off) {
            (0x33, 0xd14) => self.ram.read(off) | 1,
            (0x33, 0xc3c) => self.events,
            (0x33, 0x088) => self.rx_base & 0xf_ffff,
            (0x33, 0x08c) => self.rx_next & 0xf_ffff,
            (0x33, 0x090) => self.rx_last,
            (0x33, 0xca8) => self.txq_error & 0x7ff,
            (0x33, 0xcb0) => self.txq_complete & 0xf,
            (0x35, 0x118) => self.pwr_events,
            (0x35, 0x18) => self.tsf_latched as u32,
            (0x35, 0x1c) => (self.tsf_latched >> 32) as u32,
            (0x35, 0x128) => self.ram2.read(off),
            (0x33, _) => self.ram.read(off),
            (_, _) => self.ram2.read(off),
        };
        if self.log {
            eprintln!("[wifi] rd {:#x}+{:#05x} -> {:#010x}", block, off, v);
        }
        v
    }
    pub fn write(&mut self, block: u32, off: u32, v: u32) {
        if self.log {
            eprintln!("[wifi] wr {:#x}+{:#05x} <- {:#010x}", block, off, v);
        }
        match (block, off) {
            (0x33, 0xc40) => {
                self.events &= !v;
            }
            (0x33, 0x088) => {
                self.rx_base = DMA_ADDR_BASE | (v & 0xf_ffff);
                self.rx_handover = true;
                if self.rx_next == 0 {
                    self.rx_next = self.rx_base;
                }
                self.ram.write(off, v);
            }
            (0x33, 0x084) => {
                if v & 1 != 0 && self.rx_next == 0 {
                    self.rx_next = self.rx_base;
                }
                self.ram.write(off, v & !1);
            }
            (0x35, 0x11c) => {
                self.pwr_events &= !v;
            }
            (0x35, 0x0c) => {
                let now = (self.now_cycles / self.cycles_per_us) as i64;
                if v & 3 != 0 {
                    self.tsf_latched = (now + self.tsf_offset) as u64;
                }
                if v & (1 << 4) != 0 {
                    let set = (self.ram2.read(0x10) as u64) | ((self.ram2.read(0x14) as u64) << 32);
                    self.tsf_offset = set as i64 - now;
                }
                self.ram2.write(off, v);
            }
            (0x33, 0xca4) => {
                self.txq_error &= !(v & 0x7ff);
            }
            (0x33, 0xcac) => {
                self.txq_complete &= !(v & 0xf);
            }
            (0x33, o) if Self::txq_of(o).is_some() => {
                self.ram.write(off, v);
                if v & (1 << 31) != 0 {
                    let q = Self::txq_of(o).unwrap();
                    self.tx_pending.push((q, DMA_ADDR_BASE | (v & 0xf_ffff)));
                }
            }
            (0x33, _) => self.ram.write(off, v),
            (_, _) => self.ram2.write(off, v),
        }
    }
    pub fn tx_done(&mut self, queue: u8) {
        self.txq_complete |= 1 << queue;
        self.events |= 1 << 7;
        let o = 0xd08 - 8 * queue as u32;
        let v = self.ram.read(o);
        self.ram.write(o, v & !(3 << 30));
        let r = 0x320 - 76 * queue as u32;
        let w = self.ram2.read(r);
        self.ram2.write(r, w & !(0xf << 12));
    }
    pub fn rx_restart_at_base(&mut self) -> bool {
        if !self.rx_handover {
            return false;
        }
        self.rx_next = self.rx_base;
        self.rx_handover = false;
        true
    }
    pub fn rx_filled(&mut self, desc: u32, next: u32) {
        self.rx_last = (desc & 0xf_ffff) | (1 << 24);
        self.rx_next = next & 0xf_ffff;
        self.rx_handover = false;

        self.events |= (1 << 14) | (1 << 24);
    }
}
