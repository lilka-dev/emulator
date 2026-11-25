use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

const SECTOR: usize = 512;
const PARTITION: u64 = 2048;
const CAPACITY: u64 = 512 * 1024 * 1024;

mod catalog;
mod writeback;

struct Extent {
    path: PathBuf,
    offset: u64,
    sectors: u32,
}

struct Import {
    path: PathBuf,
    offset: u64,
}

#[derive(Default)]
struct State {
    metadata: BTreeMap<u32, Box<[u8; SECTOR]>>,
    pending: BTreeMap<u32, Box<[u8; SECTOR]>>,
    files: BTreeMap<u32, Extent>,
    importing: Option<Import>,
    data_start: u32,
}

impl State {
    fn read_sector(&self, sector: u32) -> io::Result<[u8; SECTOR]> {
        let mut bytes = [0; SECTOR];
        if let Some((&start, extent)) = self.files.range(..=sector).next_back()
            && sector - start < extent.sectors
        {
            let mut file = File::open(&extent.path)?;
            file.seek(SeekFrom::Start(
                extent.offset + u64::from(sector - start) * SECTOR as u64,
            ))?;
            let mut at = 0;
            while at < bytes.len() {
                let count = file.read(&mut bytes[at..])?;
                if count == 0 {
                    break;
                }
                at += count;
            }
            if let Some(pending) = self.pending.get(&sector) {
                bytes[at..].copy_from_slice(&pending[at..]);
            }
        } else if let Some(pending) = self.pending.get(&sector) {
            return Ok(**pending);
        } else if let Some(metadata) = self.metadata.get(&sector) {
            bytes.copy_from_slice(metadata.as_slice());
        }
        Ok(bytes)
    }

    fn map_file(&mut self, sector: u32, offset: usize, count: usize) {
        let importing = self.importing.as_mut().unwrap();
        let file_offset = importing.offset - offset as u64;
        if let Some((&first, previous)) = self.files.last_key_value()
            && first + previous.sectors == sector
            && previous.path == importing.path
            && previous.offset + u64::from(previous.sectors) * SECTOR as u64 == file_offset
        {
            self.files.last_entry().unwrap().get_mut().sectors += 1;
        } else if !self.files.contains_key(&sector) {
            self.files.insert(
                sector,
                Extent {
                    path: importing.path.clone(),
                    offset: file_offset,
                    sectors: 1,
                },
            );
        }
        importing.offset += count as u64;
    }
}

struct Volume {
    state: Arc<Mutex<State>>,
    position: u64,
}

impl Read for Volume {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let count = output
            .len()
            .min((CAPACITY - PARTITION * SECTOR as u64 - self.position) as usize);
        let state = self.state.lock().unwrap();
        let mut copied = 0;
        while copied < count {
            let absolute = self.position + PARTITION * SECTOR as u64;
            let offset = absolute as usize % SECTOR;
            let take = (SECTOR - offset).min(count - copied);
            let bytes = state.read_sector((absolute / SECTOR as u64) as u32)?;
            output[copied..copied + take].copy_from_slice(&bytes[offset..offset + take]);
            self.position += take as u64;
            copied += take;
        }
        Ok(count)
    }
}

impl Write for Volume {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        let count = input
            .len()
            .min((CAPACITY - PARTITION * SECTOR as u64 - self.position) as usize);
        let mut state = self.state.lock().unwrap();
        let mut copied = 0;
        while copied < count {
            let absolute = self.position + PARTITION * SECTOR as u64;
            let sector = (absolute / SECTOR as u64) as u32;
            let offset = absolute as usize % SECTOR;
            let take = (SECTOR - offset).min(count - copied);
            if state.importing.is_some() && sector >= state.data_start {
                state.map_file(sector, offset, take);
            } else {
                let bytes = state
                    .metadata
                    .entry(sector)
                    .or_insert_with(|| Box::new([0; SECTOR]));
                bytes[offset..offset + take].copy_from_slice(&input[copied..copied + take]);
                if bytes.iter().all(|byte| *byte == 0) {
                    state.metadata.remove(&sector);
                }
            }
            copied += take;
            self.position += take as u64;
        }
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for Volume {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let length = CAPACITY - PARTITION * SECTOR as u64;
        let next = match from {
            SeekFrom::Start(value) => i128::from(value),
            SeekFrom::Current(value) => i128::from(self.position) + i128::from(value),
            SeekFrom::End(value) => i128::from(length) + i128::from(value),
        };
        if !(0..=i128::from(length)).contains(&next) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "SD seek outside volume",
            ));
        }
        self.position = next as u64;
        Ok(self.position)
    }
}

pub struct FolderCard {
    state: Arc<Mutex<State>>,
    root: PathBuf,
    catalog: catalog::Catalog,
    metadata_dirty: bool,
    dirty_files: BTreeSet<PathBuf>,
}

impl FolderCard {
    pub fn open(root: &Path) -> io::Result<Self> {
        let root = root.canonicalize()?;
        if !root.is_dir() {
            return Err(io::Error::other("SD source is not a directory"));
        }
        let mut entries = Vec::new();
        scan(&root, &root, &mut entries, 0)?;
        let state = Arc::new(Mutex::new(State::default()));
        let volume = || Volume {
            state: state.clone(),
            position: 0,
        };
        fatfs::format_volume(
            volume(),
            fatfs::FormatVolumeOptions::new()
                .fat_type(fatfs::FatType::Fat16)
                .max_root_dir_entries(16_384)
                .bytes_per_cluster(16 * 1024)
                .volume_label(*b"LILKA SD   "),
        )?;
        {
            let mut state = state.lock().unwrap();
            let boot = state.metadata.get_mut(&(PARTITION as u32)).unwrap();
            boot[28..32].copy_from_slice(&(PARTITION as u32).to_le_bytes());
            let word = |at| u16::from_le_bytes([boot[at], boot[at + 1]]) as u32;
            state.data_start = PARTITION as u32
                + word(14)
                + u32::from(boot[16]) * word(22)
                + (word(17) * 32).div_ceil(SECTOR as u32);
            let mut mbr = Box::new([0; SECTOR]);
            mbr[450] = 6;
            mbr[454..458].copy_from_slice(&(PARTITION as u32).to_le_bytes());
            mbr[458..462]
                .copy_from_slice(&((CAPACITY / SECTOR as u64 - PARTITION) as u32).to_le_bytes());
            mbr[510..512].copy_from_slice(&[0x55, 0xaa]);
            state.metadata.insert(0, mbr);
        }
        let filesystem = fatfs::FileSystem::new(volume(), fatfs::FsOptions::new())?;
        {
            let directory = filesystem.root_dir();
            for (relative, is_directory) in entries {
                let name = relative
                    .to_str()
                    .ok_or_else(|| io::Error::other("SD filename is not UTF-8"))?;
                if is_directory {
                    directory.create_dir(name)?;
                    continue;
                }
                let source = root.join(&relative);
                let mut target = directory.create_file(name)?;
                let mut input = File::open(&source)?;
                let mut buffer = [0; 64 * 1024];
                loop {
                    let count = input.read(&mut buffer)?;
                    if count == 0 {
                        break;
                    }
                    state.lock().unwrap().importing = Some(Import {
                        path: source.clone(),
                        offset: input.stream_position()? - count as u64,
                    });
                    let written = target.write_all(&buffer[..count]);
                    state.lock().unwrap().importing = None;
                    written?;
                }
                target.flush()?;
            }
        }
        filesystem.unmount()?;
        let catalog = catalog::read(&state.lock().unwrap())?;
        Ok(Self {
            state,
            root,
            catalog,
            metadata_dirty: false,
            dirty_files: BTreeSet::new(),
        })
    }

    pub fn capacity(&self) -> u64 {
        CAPACITY
    }
    pub fn read_sector(&self, sector: u32) -> io::Result<[u8; SECTOR]> {
        if u64::from(sector) >= CAPACITY / SECTOR as u64 {
            return Err(io::Error::other("SD sector outside volume"));
        }
        self.state.lock().unwrap().read_sector(sector)
    }
    pub fn metadata_bytes(&self) -> usize {
        self.state.lock().unwrap().metadata.len() * SECTOR
    }
}

fn scan(
    root: &Path,
    directory: &Path,
    entries: &mut Vec<(PathBuf, bool)>,
    depth: usize,
) -> io::Result<()> {
    if depth > 32 || entries.len() > 10_000 {
        return Err(io::Error::other("SD directory tree is too large"));
    }
    let mut children = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    children.sort_by_key(|entry| entry.file_name());
    let mut names = BTreeSet::new();
    for entry in children {
        let kind = entry.file_type()?;
        if kind.is_symlink() || (!kind.is_dir() && !kind.is_file()) {
            return Err(io::Error::other(
                "SD folders cannot contain symbolic links or special files",
            ));
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| io::Error::other("SD filename is not UTF-8"))?;
        if !names.insert(name.to_lowercase()) {
            return Err(io::Error::other("SD filenames differ only by case"));
        }
        let path = entry.path();
        entries.push((path.strip_prefix(root).unwrap().to_owned(), kind.is_dir()));
        if kind.is_dir() {
            scan(root, &path, entries, depth + 1)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct GuestVolume {
        card: Arc<Mutex<FolderCard>>,
        position: u64,
        pending: Option<(u32, [u8; SECTOR])>,
    }
    impl GuestVolume {
        fn flush_sector(&mut self) -> io::Result<()> {
            if let Some((sector, bytes)) = self.pending.take() {
                self.card.lock().unwrap().write_sector(sector, &bytes)?;
            }
            Ok(())
        }
    }
    impl Read for GuestVolume {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let count = output
                .len()
                .min((CAPACITY - PARTITION * SECTOR as u64 - self.position) as usize);
            let mut copied = 0;
            while copied < count {
                let at = self.position + PARTITION * SECTOR as u64;
                let offset = at as usize % SECTOR;
                let take = (SECTOR - offset).min(count - copied);
                let sector = (at / SECTOR as u64) as u32;
                if self
                    .pending
                    .as_ref()
                    .is_some_and(|(pending, _)| *pending != sector)
                {
                    self.flush_sector()?;
                }
                let bytes = if let Some((_, bytes)) = &self.pending {
                    *bytes
                } else {
                    self.card.lock().unwrap().read_sector(sector)?
                };
                output[copied..copied + take].copy_from_slice(&bytes[offset..offset + take]);
                self.position += take as u64;
                copied += take;
            }
            Ok(count)
        }
    }
    impl Write for GuestVolume {
        fn write(&mut self, input: &[u8]) -> io::Result<usize> {
            let count = input
                .len()
                .min((CAPACITY - PARTITION * SECTOR as u64 - self.position) as usize);
            let mut copied = 0;
            while copied < count {
                let at = self.position + PARTITION * SECTOR as u64;
                let offset = at as usize % SECTOR;
                let take = (SECTOR - offset).min(count - copied);
                let sector = (at / SECTOR as u64) as u32;
                if self
                    .pending
                    .as_ref()
                    .is_some_and(|(pending, _)| *pending != sector)
                {
                    self.flush_sector()?;
                }
                if self.pending.is_none() {
                    self.pending = Some((sector, self.card.lock().unwrap().read_sector(sector)?));
                }
                self.pending.as_mut().unwrap().1[offset..offset + take]
                    .copy_from_slice(&input[copied..copied + take]);
                self.position += take as u64;
                copied += take;
            }
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.flush_sector()?;
            self.card.lock().unwrap().flush()
        }
    }
    impl Seek for GuestVolume {
        fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
            let next = match from {
                SeekFrom::Start(value) => i128::from(value),
                SeekFrom::Current(value) => i128::from(self.position) + i128::from(value),
                SeekFrom::End(value) => {
                    i128::from(CAPACITY - PARTITION * SECTOR as u64) + i128::from(value)
                }
            };
            if next < 0 || next > i128::from(CAPACITY - PARTITION * SECTOR as u64) {
                return Err(io::Error::other("Invalid seek"));
            }
            let sector = ((next as u64 + PARTITION * SECTOR as u64) / SECTOR as u64) as u32;
            if self
                .pending
                .as_ref()
                .is_some_and(|(pending, _)| *pending != sector)
            {
                self.flush_sector()?;
            }
            self.position = next as u64;
            Ok(self.position)
        }
    }

    #[test]
    fn guest_file_operations_update_the_host_directory() -> io::Result<()> {
        let root = std::env::temp_dir().join(format!(
            "lilka-folder-write-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root)?;
        fs::write(root.join("existing.txt"), b"prefix")?;
        let card = Arc::new(Mutex::new(FolderCard::open(&root)?));
        let filesystem = fatfs::FileSystem::new(
            GuestVolume {
                card: card.clone(),
                position: 0,
                pending: None,
            },
            fatfs::FsOptions::new(),
        )?;
        let directory = filesystem.root_dir();
        let mut file = directory.open_file("existing.txt")?;
        file.seek(SeekFrom::End(0))?;
        file.write_all(&vec![b'X'; 700])?;
        file.flush()?;
        drop(file);
        assert!(
            fs::read(root.join("existing.txt"))?
                == [b"prefix".as_slice(), &vec![b'X'; 700]].concat()
        );
        let child = directory.create_dir("saves")?;
        let mut file = child.create_file("збереження.dat")?;
        let data: Vec<_> = (0..132_384).map(|i| (i % 251) as u8).collect();
        file.write_all(&data)?;
        file.flush()?;
        drop(file);
        assert_eq!(fs::read(root.join("saves/збереження.dat"))?, data);
        directory.rename("saves", &directory, "renamed")?;
        directory.open_file("existing.txt")?.flush()?;
        assert!(!root.join("saves").exists());
        assert_eq!(fs::read(root.join("renamed/збереження.dat"))?, data);
        let renamed = directory.open_dir("renamed")?;
        renamed.remove("збереження.dat")?;
        directory.remove("renamed")?;
        directory.open_file("existing.txt")?.flush()?;
        assert!(!root.join("renamed").exists());
        drop(renamed);
        drop(child);
        drop(directory);
        filesystem.unmount()?;
        card.lock().unwrap().flush()?;
        assert_eq!(fs::read_dir(&root)?.count(), 1);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn reads_long_names_from_host_without_an_image_or_resident_file_contents() -> io::Result<()> {
        let root = std::env::temp_dir().join(format!(
            "lilka-folder-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("scripts"))?;
        let path = root.join("scripts/довга назва.lua");
        fs::write(&path, vec![b'A'; 2 * 1024 * 1024])?;
        let card = FolderCard::open(&root)?;
        assert!(card.metadata_bytes() < 256 * 1024);
        fs::write(&path, vec![b'B'; 2 * 1024 * 1024])?;
        let filesystem = fatfs::FileSystem::new(
            Volume {
                state: card.state.clone(),
                position: 0,
            },
            fatfs::FsOptions::new(),
        )?;
        let mut result = Vec::new();
        filesystem
            .root_dir()
            .open_file("scripts/довга назва.lua")?
            .read_to_end(&mut result)?;
        assert_eq!(result, vec![b'B'; 2 * 1024 * 1024]);
        filesystem.unmount()?;
        assert_eq!(fs::read_dir(&root)?.count(), 1);
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
