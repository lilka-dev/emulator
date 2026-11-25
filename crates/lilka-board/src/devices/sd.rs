use std::collections::VecDeque;

pub const ROOT_SECTOR: usize = 130;
pub const DATA_SECTOR: usize = 138;

fn crc7(bytes: &[u8]) -> u8 {
    let mut crc = 0_u8;
    for &byte in bytes {
        let mut data = byte;
        for _ in 0..8 {
            crc <<= 1;
            if (data ^ crc) & 0x80 != 0 {
                crc ^= 0x09;
            }
            data <<= 1;
        }
    }
    (crc << 1) | 1
}

fn crc16(bytes: &[u8]) -> u16 {
    let mut crc = 0_u16;
    for &byte in bytes {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

pub fn demo_image() -> Vec<u8> {
    let mut image = vec![0_u8; 8 * 1024 * 1024];
    image[446 + 4] = 0x06;
    image[454..458].copy_from_slice(&1_u32.to_le_bytes());
    image[458..462].copy_from_slice(&16383_u32.to_le_bytes());
    image[510..512].copy_from_slice(&[0x55, 0xaa]);
    let boot = &mut image[512..1024];
    boot[..3].copy_from_slice(&[0xeb, 0x3c, 0x90]);
    boot[3..11].copy_from_slice(b"LILKAEMU");
    boot[11..13].copy_from_slice(&512_u16.to_le_bytes());
    boot[13] = 1;
    boot[14..16].copy_from_slice(&1_u16.to_le_bytes());
    boot[16] = 2;
    boot[17..19].copy_from_slice(&128_u16.to_le_bytes());
    boot[19..21].copy_from_slice(&16383_u16.to_le_bytes());
    boot[21] = 0xf8;
    boot[22..24].copy_from_slice(&64_u16.to_le_bytes());
    boot[24..26].copy_from_slice(&32_u16.to_le_bytes());
    boot[26..28].copy_from_slice(&64_u16.to_le_bytes());
    boot[28..32].copy_from_slice(&1_u32.to_le_bytes());
    boot[36] = 0x80;
    boot[38] = 0x29;
    boot[39..43].copy_from_slice(&0x4c494c4b_u32.to_le_bytes());
    boot[43..54].copy_from_slice(b"LILKA SD   ");
    boot[54..62].copy_from_slice(b"FAT16   ");
    boot[510..512].copy_from_slice(&[0x55, 0xaa]);
    for sector in [2, 66] {
        image[sector * 512..sector * 512 + 8]
            .copy_from_slice(&[0xf8, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
    }
    let text = b"Hello from Lilka SD!\r\n";
    for (index, name, cluster, size) in [
        (0, b"HELLO   TXT", 2_u16, text.len() as u32),
        (1, b"TEST    TXT", 3_u16, 512_u32),
    ] {
        let entry =
            &mut image[ROOT_SECTOR * 512 + index * 32..ROOT_SECTOR * 512 + (index + 1) * 32];
        entry[..11].copy_from_slice(name);
        entry[11] = 0x20;
        entry[26..28].copy_from_slice(&cluster.to_le_bytes());
        entry[28..32].copy_from_slice(&size.to_le_bytes());
    }
    image[DATA_SECTOR * 512..DATA_SECTOR * 512 + text.len()].copy_from_slice(text);
    image
}

#[cfg(not(target_arch = "wasm32"))]
pub mod folder;

enum WriteState {
    Token(usize),
    Bytes { sector: usize, bytes: Vec<u8> },
}

pub struct SdCard {
    pub image: Vec<u8>,
    #[cfg(not(target_arch = "wasm32"))]
    file: Option<std::fs::File>,
    #[cfg(not(target_arch = "wasm32"))]
    folder: Option<std::sync::Mutex<folder::FolderCard>>,
    #[cfg(not(target_arch = "wasm32"))]
    file_writable: bool,
    #[cfg(not(target_arch = "wasm32"))]
    read_cache: Option<(usize, Vec<u8>)>,
    size: usize,
    pub io_error: Option<String>,
    selected: bool,
    idle: bool,
    app_command: bool,
    crc_enabled: bool,
    spi_mode: bool,
    write_run_count: u32,
    erase_start: Option<usize>,
    erase_end: Option<usize>,
    command: Vec<u8>,
    response: VecDeque<u8>,
    write: Option<WriteState>,
    multi_write: bool,
    read_next: Option<usize>,
    pub blocks_read: usize,
    pub blocks_written: usize,
    pub file_read_batches: u64,
    pub file_cached_sector_reads: u64,
}

impl SdCard {
    pub fn demo() -> Self {
        Self::from_image(demo_image()).expect("valid demo image")
    }
    pub fn from_image(image: Vec<u8>) -> anyhow::Result<Self> {
        anyhow::ensure!(
            image.len() >= 512 * 1024 && image.len().is_multiple_of(512 * 1024),
            "SDHC image size must be a positive multiple of 512 KiB"
        );
        Ok(Self {
            size: image.len(),
            image,
            #[cfg(not(target_arch = "wasm32"))]
            file: None,
            #[cfg(not(target_arch = "wasm32"))]
            folder: None,
            #[cfg(not(target_arch = "wasm32"))]
            file_writable: false,
            #[cfg(not(target_arch = "wasm32"))]
            read_cache: None,
            io_error: None,
            selected: false,
            idle: true,
            app_command: false,
            crc_enabled: false,
            write_run_count: 0,
            spi_mode: false,
            erase_start: None,
            erase_end: None,
            command: Vec::new(),
            response: VecDeque::new(),
            write: None,
            multi_write: false,
            read_next: None,
            blocks_read: 0,
            blocks_written: 0,
            file_read_batches: 0,
            file_cached_sector_reads: 0,
        })
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub fn from_folder(path: &std::path::Path) -> anyhow::Result<Self> {
        let folder = folder::FolderCard::open(path)?;
        let mut card = Self::from_image(vec![0; 512 * 1024])?;
        card.image = Vec::new();
        card.size = folder.capacity() as usize;
        card.folder = Some(std::sync::Mutex::new(folder));
        Ok(card)
    }
    pub fn is_folder_backed(&self) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.folder.is_some()
        }
        #[cfg(target_arch = "wasm32")]
        {
            false
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub fn from_file(path: &std::path::Path, writable: bool) -> anyhow::Result<Self> {
        let limit = if std::env::var_os("LILKA_SD_STREAM_ONLY").is_some() {
            0
        } else {
            64 * 1024 * 1024
        };
        Self::from_file_with_cache_limit(path, writable, limit)
    }
    #[cfg(not(target_arch = "wasm32"))]
    fn from_file_with_cache_limit(
        path: &std::path::Path,
        writable: bool,
        cache_limit: u64,
    ) -> anyhow::Result<Self> {
        use std::io::Read;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(writable)
            .open(path)?;
        let meta = file.metadata()?;
        anyhow::ensure!(
            meta.is_file(),
            "SD file backend requires a regular image file"
        );
        let size = meta.len();
        anyhow::ensure!(
            size >= 512 * 1024
                && size.is_multiple_of(512 * 1024)
                && size <= 2_u64 * 1024 * 1024 * 1024 * 1024,
            "SDHC image must be 512 KiB aligned and at most 2 TiB"
        );
        let mut card = Self::from_image(vec![0; 512 * 1024])?;
        card.size = usize::try_from(size)?;
        if size <= cache_limit {
            card.image.resize(card.size, 0);
            file.read_exact(&mut card.image)?;
            card.file_read_batches = 1;
        } else {
            card.image = Vec::new();
        }
        card.file = Some(file);
        card.file_writable = writable;
        Ok(card)
    }
    pub fn capacity(&self) -> usize {
        self.size
    }
    pub fn is_file_backed(&self) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.file.is_some()
        }
        #[cfg(target_arch = "wasm32")]
        {
            false
        }
    }
    pub fn flush(&self) -> anyhow::Result<()> {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(folder) = &self.folder {
            folder.lock().unwrap().flush()?;
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(file) = &self.file
            && self.file_writable
        {
            file.sync_data()?;
        }
        Ok(())
    }
    pub fn reset_bus(&mut self) {
        self.selected = false;
        self.idle = true;
        self.app_command = false;
        self.crc_enabled = false;
        self.spi_mode = false;
        self.write_run_count = 0;
        self.erase_start = None;
        self.erase_end = None;
        self.command.clear();
        self.response.clear();
        self.write = None;
        self.multi_write = false;
        self.read_next = None;
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub fn export(&self, path: &std::path::Path) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.is_folder_backed(),
            "Folder SD cards store files directly; no image export is needed"
        );
        use std::io::{Read, Seek, SeekFrom, Write};
        if let Some(file) = &self.file {
            if path.exists() {
                let source = file.metadata()?;
                let target = std::fs::metadata(path)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    anyhow::ensure!(
                        source.dev() != target.dev() || source.ino() != target.ino(),
                        "Оберіть інший файл для експорту"
                    );
                }
                #[cfg(not(unix))]
                {
                    let _ = (source, target);
                    anyhow::bail!("Оберіть новий файл для експорту");
                }
            }
            self.flush()?;
            let mut input = file.try_clone()?;
            input.seek(SeekFrom::Start(0))?;
            let mut output = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?;
            std::io::copy(
                &mut std::io::Read::by_ref(&mut input).take(self.size as u64),
                &mut output,
            )?;
            output.sync_all()?;
        } else {
            let mut output = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?;
            output.write_all(&self.image)?;
            output.sync_all()?;
        }
        Ok(())
    }
    fn read_sector(&mut self, sector: usize) -> Option<[u8; 512]> {
        let offset = sector.checked_mul(512)?;
        if offset.checked_add(512)? > self.size {
            return None;
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(folder) = &mut self.folder {
            return match folder.get_mut().unwrap().read_sector(sector as u32) {
                Ok(bytes) => Some(bytes),
                Err(error) => {
                    self.io_error = Some(error.to_string());
                    None
                }
            };
        }
        let mut bytes = [0; 512];
        if self.image.len() == self.size {
            bytes.copy_from_slice(&self.image[offset..offset + 512]);
            #[cfg(not(target_arch = "wasm32"))]
            if self.file.is_some() {
                self.file_cached_sector_reads += 1;
            }
            return Some(bytes);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(file) = &mut self.file {
            use std::io::{Read, Seek, SeekFrom};
            if let Some((first, cache)) = &self.read_cache
                && sector >= *first
                && (sector - first) < cache.len() / 512
            {
                let at = (sector - first) * 512;
                bytes.copy_from_slice(&cache[at..at + 512]);
                return Some(bytes);
            }
            let first = sector / 32 * 32;
            let count = 32.min(self.size / 512 - first);
            let mut cache = self
                .read_cache
                .take()
                .map_or_else(Vec::new, |(_, bytes)| bytes);
            cache.resize(count * 512, 0);
            self.file_read_batches += 1;
            if let Err(error) = file
                .seek(SeekFrom::Start(first as u64 * 512))
                .and_then(|_| file.read_exact(&mut cache))
            {
                self.io_error = Some(error.to_string());
                return None;
            }
            let at = (sector - first) * 512;
            bytes.copy_from_slice(&cache[at..at + 512]);
            self.read_cache = Some((first, cache));
            return Some(bytes);
        }
        None
    }
    fn write_sector(&mut self, sector: usize, bytes: &[u8]) -> bool {
        if bytes.len() != 512
            || sector
                .checked_mul(512)
                .and_then(|at| at.checked_add(512))
                .is_none_or(|end| end > self.size)
        {
            return false;
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.read_cache = None;
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(folder) = &mut self.folder {
            return match folder
                .get_mut()
                .unwrap()
                .write_sector(sector as u32, bytes.try_into().unwrap())
            {
                Ok(()) => true,
                Err(error) => {
                    self.io_error = Some(error.to_string());
                    false
                }
            };
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(file) = &mut self.file {
            use std::io::{Seek, SeekFrom, Write};
            let result = file
                .seek(SeekFrom::Start(sector as u64 * 512))
                .and_then(|_| file.write_all(bytes));
            if let Err(error) = result {
                self.io_error = Some(error.to_string());
                return false;
            }
            if self.image.len() == self.size {
                self.image[sector * 512..(sector + 1) * 512].copy_from_slice(bytes);
            }
            return true;
        }
        self.image[sector * 512..(sector + 1) * 512].copy_from_slice(bytes);
        true
    }
    fn erase(&mut self, start: usize, end: usize) -> bool {
        if self.is_folder_backed() {
            return false;
        }
        #[cfg(not(target_arch = "wasm32"))]
        if self.file.is_some() {
            for sector in start..=end {
                if !self.write_sector(sector, &[0xff; 512]) {
                    return false;
                }
            }
            return true;
        }
        self.image[start * 512..(end + 1) * 512].fill(0xff);
        true
    }
    pub fn select(&mut self, selected: bool) {
        if self.selected && !selected {
            self.command.clear();
        }
        self.selected = selected;
    }
    fn data(&mut self, bytes: &[u8]) {
        self.response.extend([0xff, 0xfe]);
        self.response.extend(bytes);
        self.response.extend(crc16(bytes).to_be_bytes());
    }
    pub fn clock(&mut self, mosi: u8) -> u8 {
        if !self.selected {
            return 0xff;
        }
        if self.response.is_empty()
            && self.command.is_empty()
            && mosi & 0xc0 != 0x40
            && let Some(sector) = self.read_next
        {
            if sector < self.size / 512 {
                if let Some(bytes) = self.read_sector(sector) {
                    self.data(&bytes);
                    self.blocks_read += 1;
                    self.read_next = Some(sector + 1);
                } else {
                    self.response.push_back(0x08);
                    self.read_next = None;
                }
            } else {
                self.response.push_back(0x08);
                self.read_next = None;
            }
        }
        let miso = self.response.pop_front().unwrap_or(0xff);
        if let Some(write) = self.write.take() {
            self.write = match write {
                WriteState::Token(_) if self.multi_write && mosi == 0xfd => {
                    self.multi_write = false;
                    self.response.extend([0, 0xff]);
                    None
                }
                WriteState::Token(sector) if mosi == if self.multi_write { 0xfc } else { 0xfe } => {
                    Some(WriteState::Bytes {
                        sector,
                        bytes: Vec::with_capacity(514),
                    })
                }
                WriteState::Token(sector) => Some(WriteState::Token(sector)),
                WriteState::Bytes { sector, mut bytes } => {
                    bytes.push(mosi);
                    if bytes.len() == 514 {
                        let mut next_sector = sector;
                        if self.crc_enabled
                            && crc16(&bytes[..512]) != u16::from_be_bytes([bytes[512], bytes[513]])
                        {
                            self.response.extend([0x0b, 0xff]);
                        } else if sector >= self.size / 512
                            || !self.write_sector(sector, &bytes[..512])
                        {
                            self.response.extend([0x0d, 0xff]);
                        } else {
                            self.blocks_written += 1;
                            self.write_run_count += 1;
                            next_sector = sector + 1;
                            self.response.extend([0x05, 0x00, 0xff]);
                        }
                        if self.multi_write {
                            Some(WriteState::Token(next_sector))
                        } else {
                            None
                        }
                    } else {
                        Some(WriteState::Bytes { sector, bytes })
                    }
                }
            };
            return miso;
        }
        if self.command.is_empty() && mosi & 0xc0 != 0x40 {
            return miso;
        }
        self.command.push(mosi);
        if self.command.len() == 6 {
            let number = self.command[0] & 63;
            let argument = u32::from_be_bytes(self.command[1..5].try_into().unwrap());
            let valid = crc7(&self.command[..5]) == self.command[5];
            self.command.clear();
            if (self.crc_enabled || number == 8 || (number == 0 && !self.spi_mode)) && !valid {
                self.app_command = false;
                self.response.push_back(u8::from(self.idle) | 8);
            } else {
                self.execute(number, argument);
            }
        }
        miso
    }
    fn execute(&mut self, number: u8, argument: u32) {
        let app = self.app_command;
        self.app_command = false;
        let status = u8::from(self.idle);
        match number {
            0 => {
                self.spi_mode = true;
                self.erase_start = None;
                self.erase_end = None;
                self.idle = true;
                self.crc_enabled = false;
                self.multi_write = false;
                self.read_next = None;
                self.write = None;
                self.response.clear();
                self.response.push_back(1);
            }
            8 => {
                self.response.push_back(status);
                self.response.extend(argument.to_be_bytes());
            }
            55 => {
                self.app_command = true;
                self.response.push_back(status);
            }
            41 if app => {
                self.idle = false;
                self.response.push_back(0);
            }
            12 if !self.idle => {
                self.read_next = None;
                self.response.clear();
                self.response.extend([0xff, 0, 0, 0xff]);
            }
            23 if app && !self.idle => self.response.push_back(0),
            42 if app && !self.idle => self.response.push_back(0),
            58 => {
                self.response.push_back(status);
                self.response.extend(
                    (if self.idle {
                        0x00ff8000_u32
                    } else {
                        0xc0ff8000_u32
                    })
                    .to_be_bytes(),
                );
            }
            16 => self
                .response
                .push_back(status | if argument == 512 { 0 } else { 0x40 }),
            59 => {
                self.crc_enabled = argument & 1 != 0;
                self.response.push_back(status);
            }
            13 if app && !self.idle => {
                self.response.extend([0, 0]);
                self.data(&[0; 64]);
            }
            13 => self.response.extend([status, 0]),
            51 if app && !self.idle => {
                self.response.push_back(0);
                self.data(&[2, 0x85, 0, 0, 0, 0, 0, 0]);
            }
            22 if app && !self.idle => {
                self.response.push_back(0);
                self.data(&(self.write_run_count).to_be_bytes());
            }
            6 if !app && !self.idle => {
                self.response.push_back(0);
                let mut data = [0; 64];
                data[13] = 1;
                self.data(&data);
            }
            32 | 33 if !self.idle => {
                let sector = argument as usize;
                if sector >= self.size / 512 {
                    self.response.push_back(0x20);
                } else {
                    if number == 32 {
                        self.erase_start = Some(sector);
                        self.erase_end = None;
                    } else {
                        self.erase_end = Some(sector);
                    }
                    self.response.push_back(0);
                }
            }
            38 if !self.idle => {
                if let (Some(start), Some(end)) = (self.erase_start, self.erase_end)
                    && start <= end
                {
                    if self.erase(start, end) {
                        self.response.extend([0, 0, 0xff]);
                    } else {
                        self.response.push_back(0x40);
                    }
                    self.erase_start = None;
                    self.erase_end = None;
                } else {
                    self.response.push_back(0x40);
                }
            }
            9 | 10 if !self.idle => {
                self.response.push_back(0);
                let mut data = [0; 16];
                if number == 9 {
                    data[0] = 0x40;
                    data[1] = 0x0e;
                    data[3] = 0x32;
                    data[4] = 0x43;
                    data[5] = 0x59;
                    data[10] = 0x7f;
                    data[11] = 0x80;
                    data[12] = 0x0a;
                    data[13] = 0x40;
                    let size = (self.size / (512 * 1024) - 1) as u32;
                    data[7] = ((size >> 16) & 0x3f) as u8;
                    data[8] = (size >> 8) as u8;
                    data[9] = size as u8;
                } else {
                    data[..8].copy_from_slice(b"\x03EMLILKA");
                    data[8] = 0x10;
                    data[9..13].copy_from_slice(b"LILK");
                    data[13] = 1;
                    data[14] = 0xaa;
                }
                data[15] = crc7(&data[..15]);
                self.data(&data);
            }
            17 | 18 | 24 | 25 if !self.idle => {
                let sector = argument as usize;
                if sector >= self.size / 512 {
                    self.response.push_back(0x20);
                } else {
                    self.response.push_back(0);
                    if number == 18 {
                        self.read_next = Some(sector);
                    } else if number == 17 {
                        if let Some(bytes) = self.read_sector(sector) {
                            self.data(&bytes);
                            self.blocks_read += 1;
                        } else {
                            self.response.push_back(0x08);
                        }
                    } else {
                        self.write_run_count = 0;
                        self.multi_write = number == 25;
                        self.write = Some(WriteState::Token(sector));
                    }
                }
            }
            _ => self.response.push_back(status | 4),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn small_file_cache_keeps_writes_erase_and_reopen_coherent() -> anyhow::Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        let path = std::env::temp_dir().join(format!(
            "lilka-sd-cache-{}-{}.img",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        struct Remove(std::path::PathBuf);
        impl Drop for Remove {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _remove = Remove(path.clone());
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?
            .set_len(512 * 1024)?;
        let mut card = SdCard::from_file_with_cache_limit(&path, true, 512 * 1024)?;
        assert!(card.is_file_backed());
        for sector in [0, 900, 1, 700, 0, 900] {
            assert_eq!(card.read_sector(sector), Some([0; 512]));
        }
        assert_eq!(card.file_read_batches, 1);
        assert_eq!(card.file_cached_sector_reads, 6);
        assert!(card.write_sector(900, &[0x5a; 512]));
        assert_eq!(card.read_sector(900), Some([0x5a; 512]));
        assert!(card.erase(900, 900));
        assert_eq!(card.read_sector(900), Some([0xff; 512]));
        card.flush()?;
        drop(card);
        let mut file = std::fs::File::open(&path)?;
        file.seek(SeekFrom::Start(900 * 512))?;
        let mut bytes = [0; 512];
        file.read_exact(&mut bytes)?;
        assert_eq!(bytes, [0xff; 512]);
        let mut readonly = SdCard::from_file_with_cache_limit(&path, false, 512 * 1024)?;
        assert!(!readonly.write_sector(900, &[0x11; 512]));
        assert_eq!(readonly.read_sector(900), Some([0xff; 512]));
        Ok(())
    }
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn sparse_four_gib_file_reads_writes_last_sector_without_ram_image() -> anyhow::Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        let path = std::env::temp_dir().join(format!(
            "lilka-sd-{}-{}.img",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _cleanup = Cleanup(path.clone());
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.set_len(4 * 1024 * 1024 * 1024)?;
        drop(file);
        let mut card = SdCard::from_file(&path, true)?;
        assert!(card.image.is_empty());
        assert_eq!(card.capacity(), 4 * 1024 * 1024 * 1024);
        card.select(true);
        assert_eq!(command(&mut card, 0, 0), 1);
        command(&mut card, 55, 0);
        assert_eq!(command(&mut card, 41, 0x40000000), 0);
        let last = (card.capacity() / 512 - 1) as u32;
        assert_eq!(command(&mut card, 24, last), 0);
        card.clock(0xfe);
        for _ in 0..512 {
            card.clock(0xa5);
        }
        card.clock(0xff);
        card.clock(0xff);
        assert_eq!(card.clock(0xff) & 0x1f, 5);
        for _ in 0..3 {
            card.clock(0xff);
        }
        assert_eq!(command(&mut card, 17, last), 0);
        while card.clock(0xff) != 0xfe {}
        for _ in 0..512 {
            assert_eq!(card.clock(0xff), 0xa5);
        }
        card.flush()?;
        drop(card);
        let mut file = std::fs::File::open(&path)?;
        file.seek(SeekFrom::End(-512))?;
        let mut bytes = [0; 512];
        file.read_exact(&mut bytes)?;
        assert_eq!(bytes, [0xa5; 512]);
        let mut readonly = SdCard::from_file(&path, false)?;
        assert!(!readonly.write_sector(last as usize, &[0; 512]));
        assert_eq!(readonly.read_sector(last as usize), Some([0xa5; 512]));
        Ok(())
    }
    fn command(card: &mut SdCard, cmd: u8, arg: u32) -> u8 {
        let mut packet = [
            0x40 | cmd,
            (arg >> 24) as u8,
            (arg >> 16) as u8,
            (arg >> 8) as u8,
            arg as u8,
            0,
        ];
        packet[5] = crc7(&packet[..5]);
        for byte in packet {
            card.clock(byte);
        }
        card.clock(0xff)
    }
    #[test]
    fn crc7_guards_commands_and_crc_failures_do_not_change_state() {
        assert_eq!(crc7(&[0x40, 0, 0, 0, 0]), 0x95);
        assert_eq!(crc7(&[0x48, 0, 0, 1, 0xaa]), 0x87);
        let mut card = SdCard::demo();
        card.select(true);
        for b in [0x40, 0, 0, 0, 0, 0] {
            card.clock(b);
        }
        assert_eq!(card.clock(0xff), 9);
        assert_eq!(command(&mut card, 0, 0), 1);
        assert_eq!(command(&mut card, 59, 1), 1);
        for b in [0x77, 0, 0, 0, 0, 1] {
            card.clock(b);
        }
        assert_eq!(card.clock(0xff), 9);
        assert_eq!(command(&mut card, 41, 0x40000000), 5);
        assert_eq!(command(&mut card, 55, 0), 1);
        assert_eq!(command(&mut card, 41, 0x40000000), 0);
    }
    #[test]
    fn scr_and_erase_are_observable_over_spi() {
        let mut card = SdCard::demo();
        card.select(true);
        command(&mut card, 55, 0);
        command(&mut card, 41, 0x40000000);
        assert_eq!(command(&mut card, 55, 0), 0);
        assert_eq!(command(&mut card, 51, 0), 0);
        assert_eq!(card.clock(0xff), 0xff);
        assert_eq!(card.clock(0xff), 0xfe);
        let mut scr = [0; 8];
        for b in &mut scr {
            *b = card.clock(0xff);
        }
        assert_eq!(&scr[..2], &[2, 0x85]);
        assert_eq!(
            u16::from_be_bytes([card.clock(0xff), card.clock(0xff)]),
            crc16(&scr)
        );
        let before = card.image[139 * 512..140 * 512].to_vec();
        assert_eq!(command(&mut card, 32, 138), 0);
        assert_eq!(command(&mut card, 33, 138), 0);
        assert_eq!(command(&mut card, 38, 0), 0);
        card.clock(0xff);
        card.clock(0xff);
        assert!(card.image[138 * 512..139 * 512].iter().all(|&b| b == 0xff));
        assert_eq!(&card.image[139 * 512..140 * 512], &before);
    }
    #[test]
    fn incomplete_write_and_out_of_range_read_do_not_change_the_image() {
        let mut card = SdCard::demo();
        card.select(true);
        assert_eq!(command(&mut card, 55, 0), 1);
        assert_eq!(command(&mut card, 41, 0x40000000), 0);
        assert_eq!(command(&mut card, 17, u32::MAX), 0x20);
        let before = card.image.clone();
        assert_eq!(command(&mut card, 24, 139), 0);
        card.clock(0xfe);
        for _ in 0..200 {
            card.clock(0xa5);
        }
        card.select(false);
        assert_eq!(card.image, before);
        assert_eq!(card.blocks_written, 0);
    }
    #[test]
    fn command_and_response_survive_arbitrary_chunking_and_cs_clears_partial_command() {
        let mut card = SdCard::demo();
        card.select(true);
        card.clock(0x48);
        card.clock(0);
        card.select(false);
        card.select(true);
        assert_eq!(command(&mut card, 0, 0), 1);
        assert_eq!(command(&mut card, 8, 0x1aa), 1);
        let response: Vec<_> = (0..4).map(|_| card.clock(0xff)).collect();
        assert_eq!(response, [0, 0, 1, 0xaa]);
    }

    #[test]
    fn bad_crc_write_is_rejected_without_modifying_storage() {
        let mut card = SdCard::demo();
        card.select(true);
        assert_eq!(command(&mut card, 55, 0), 1);
        assert_eq!(command(&mut card, 41, 0x40000000), 0);
        assert_eq!(command(&mut card, 59, 1), 0);
        let before = card.image.clone();
        assert_eq!(command(&mut card, 24, 139), 0);
        card.clock(0xfe);
        for _ in 0..512 {
            card.clock(0xa5);
        }
        card.clock(0);
        card.clock(0);
        assert_eq!(card.clock(0xff), 0x0b);
        assert_eq!(card.image, before);
        assert_eq!(card.blocks_written, 0);
    }
    #[test]
    fn multiblock_roundtrip_and_stop_tokens_use_real_sector_streams() {
        let mut card = SdCard::demo();
        card.select(true);
        assert_eq!(command(&mut card, 55, 0), 1);
        assert_eq!(command(&mut card, 41, 0x40000000), 0);
        assert_eq!(command(&mut card, 55, 0), 0);
        assert_eq!(command(&mut card, 42, 0), 0);
        assert_eq!(command(&mut card, 55, 0), 0);
        assert_eq!(command(&mut card, 23, 2), 0);
        assert_eq!(command(&mut card, 25, 140), 0);
        for value in [0x31, 0x32] {
            card.clock(0xfc);
            for _ in 0..512 {
                card.clock(value);
            }
            card.clock(0);
            card.clock(0);
            assert_eq!(card.clock(0xff), 5);
            assert_eq!(card.clock(0xff), 0);
            assert_eq!(card.clock(0xff), 0xff);
        }
        card.clock(0xfd);
        card.clock(0xff);
        card.clock(0xff);
        assert_eq!(command(&mut card, 18, 140), 0);
        for value in [0x31, 0x32] {
            assert_eq!(card.clock(0xff), 0xff);
            assert_eq!(card.clock(0xff), 0xfe);
            let bytes: Vec<_> = (0..512).map(|_| card.clock(0xff)).collect();
            assert!(bytes.iter().all(|b| *b == value));
            assert_eq!(
                u16::from_be_bytes([card.clock(0xff), card.clock(0xff)]),
                crc16(&bytes)
            );
        }
        assert_eq!(command(&mut card, 12, 0), 0xff);
        assert_eq!(card.clock(0xff), 0);
        assert!(card.read_next.is_none());
        assert_eq!(card.blocks_read, 2);
        assert_eq!(card.blocks_written, 2);
    }
}
