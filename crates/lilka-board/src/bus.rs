use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Fetch,
    Read,
    Write,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryFault {
    pub address: u32,
    pub access: Access,
}
impl fmt::Display for MemoryFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} at {:#010x}", self.access, self.address)
    }
}
impl std::error::Error for MemoryFault {}

pub trait Bus {
    fn read_bytes(&mut self, address: u32, out: &mut [u8]) -> Result<(), MemoryFault> {
        for (i, byte) in out.iter_mut().enumerate() {
            let at = address
                .checked_add(u32::try_from(i).map_err(|_| MemoryFault {
                    address,
                    access: Access::Read,
                })?)
                .ok_or(MemoryFault {
                    address,
                    access: Access::Read,
                })?;
            *byte = self.read(at, 1, Access::Read)? as u8;
        }
        Ok(())
    }
    fn write_bytes(&mut self, address: u32, bytes: &[u8]) -> Result<(), MemoryFault> {
        for (i, byte) in bytes.iter().enumerate() {
            let at = address
                .checked_add(u32::try_from(i).map_err(|_| MemoryFault {
                    address,
                    access: Access::Write,
                })?)
                .ok_or(MemoryFault {
                    address,
                    access: Access::Write,
                })?;
            self.write(at, 1, u32::from(*byte))?;
        }
        Ok(())
    }
    fn read(&mut self, address: u32, width: u8, access: Access) -> Result<u32, MemoryFault>;
    fn write(&mut self, address: u32, width: u8, value: u32) -> Result<(), MemoryFault>;
}

pub trait Device: Send {
    fn reads_interrupt_sources(&self, _offset: u32) -> bool {
        false
    }
    fn set_interrupt_sources(&mut self, _sources: u128) {}
    fn read_changes_events(&self, _offset: u32) -> bool {
        true
    }
    fn write_changes_events(&self, _offset: u32, _width: u8) -> bool {
        true
    }
    fn advance(&mut self, _now: u64, _bus: &mut dyn Bus) {}
    fn next_event(&self, _now: u64) -> Option<u64> {
        None
    }
    fn interrupt_sources(&mut self, _now: u64) -> u128 {
        0
    }
    fn write_with_bus(
        &mut self,
        offset: u32,
        width: u8,
        value: u32,
        now: u64,
        _bus: &mut dyn Bus,
    ) -> bool {
        self.write(offset, width, value, now)
    }
    fn read(&mut self, offset: u32, width: u8, now: u64) -> Option<u32>;
    fn write(&mut self, offset: u32, width: u8, value: u32, now: u64) -> bool;
}

#[cfg(test)]
pub mod tests {
    use super::*;
    #[derive(Default)]
    pub struct Memory;
    impl Bus for Memory {
        fn read(&mut self, address: u32, _: u8, access: Access) -> Result<u32, MemoryFault> {
            Err(MemoryFault { address, access })
        }
        fn write(&mut self, address: u32, _: u8, _: u32) -> Result<(), MemoryFault> {
            Err(MemoryFault {
                address,
                access: Access::Write,
            })
        }
    }
}
