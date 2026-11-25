use super::*;
use std::fs::OpenOptions;

impl FolderCard {
    fn checked_path(&self, relative: &Path) -> io::Result<PathBuf> {
        let mut path = self.root.clone();
        for component in relative.components() {
            let std::path::Component::Normal(name) = component else {
                return Err(io::Error::other("Invalid SD path"));
            };
            path.push(name);
            match path.symlink_metadata() {
                Ok(metadata)
                    if metadata.file_type().is_symlink()
                        || (!metadata.is_dir() && !metadata.is_file()) =>
                {
                    return Err(io::Error::other("SD path became a symbolic link"));
                }
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
        }
        Ok(path)
    }

    pub fn write_sector(&mut self, sector: u32, bytes: &[u8; SECTOR]) -> io::Result<()> {
        if u64::from(sector) >= CAPACITY / SECTOR as u64 {
            return Err(io::Error::other("SD sector outside volume"));
        }
        let directory = self.catalog.sectors.contains(&sector);
        {
            let mut state = self.state.lock().unwrap();
            if sector == PARTITION as u32 && bytes[11..36] != state.read_sector(sector)?[11..36] {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "Formatting a host SD folder is not supported",
                ));
            }
            if sector < state.data_start || directory {
                state.metadata.insert(sector, Box::new(*bytes));
                self.metadata_dirty = true;
            } else {
                let mapped =
                    state
                        .files
                        .range(..=sector)
                        .next_back()
                        .and_then(|(&start, extent)| {
                            (sector - start < extent.sectors).then(|| {
                                (
                                    extent.path.clone(),
                                    extent.offset + u64::from(sector - start) * SECTOR as u64,
                                )
                            })
                        });
                let mut complete = false;
                if let Some((path, offset)) = mapped {
                    let relative = path.strip_prefix(&self.root).map_err(io::Error::other)?;
                    let path = self.checked_path(relative)?;
                    let mut file = OpenOptions::new().write(true).open(&path)?;
                    let length = file.metadata()?.len();
                    if offset < length {
                        let count = (length - offset).min(SECTOR as u64) as usize;
                        file.seek(SeekFrom::Start(offset))?;
                        file.write_all(&bytes[..count])?;
                        self.dirty_files.insert(path);
                        complete = count == SECTOR;
                    }
                }
                if !complete {
                    if state.pending.len() >= 65536 && !state.pending.contains_key(&sector) {
                        return Err(io::Error::new(
                            io::ErrorKind::OutOfMemory,
                            "Too much uncommitted SD data",
                        ));
                    }
                    state.pending.insert(sector, Box::new(*bytes));
                }
            }
        }
        if directory {
            match self.reconcile() {
                Err(error) if error.kind() == io::ErrorKind::InvalidData => {}
                result => result?,
            }
        }
        Ok(())
    }

    fn reconcile(&mut self) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        let next = catalog::read(&state)?;
        let mut renames = BTreeMap::new();
        for (path, node) in &next.entries {
            if node.directory || self.catalog.entries.contains_key(path) {
                continue;
            }
            if let Some((old_path, _)) = self.catalog.entries.iter().find(|(old_path, old)| {
                !old.directory
                    && !next.entries.contains_key(*old_path)
                    && !node.clusters.is_empty()
                    && old.clusters.first() == node.clusters.first()
            }) {
                renames.insert(path.clone(), old_path.clone());
            }
        }
        for (path, node) in &next.entries {
            let target = self.checked_path(path)?;
            if !self.catalog.entries.contains_key(path) && target.try_exists()? {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("SD host entry already exists: {}", target.display()),
                ));
            }
            if self
                .catalog
                .entries
                .get(path)
                .is_some_and(|old| old.directory != node.directory)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "SD entry changed type before deletion",
                ));
            }
        }
        let mut directories: Vec<_> = next
            .entries
            .iter()
            .filter(|(_, node)| node.directory)
            .map(|(path, _)| path)
            .collect();
        directories.sort_by_key(|path| path.components().count());
        for path in directories {
            fs::create_dir_all(self.checked_path(path)?)?;
        }
        for (new, old) in &renames {
            fs::rename(self.checked_path(old)?, self.checked_path(new)?)?;
        }
        let mut extents: BTreeMap<u32, Extent> = BTreeMap::new();
        for (relative, node) in &next.entries {
            if node.directory {
                continue;
            }
            let path = self.checked_path(relative)?;
            let existed =
                self.catalog.entries.contains_key(relative) || renames.contains_key(relative);
            let mut file = if existed {
                None
            } else {
                Some(
                    OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&path)?,
                )
            };
            if fs::metadata(&path)?.len() != u64::from(node.size) {
                let output = match &mut file {
                    Some(output) => output,
                    None => file.insert(OpenOptions::new().write(true).open(&path)?),
                };
                output.set_len(u64::from(node.size))?;
                self.dirty_files.insert(path.clone());
            }
            let mut offset = 0;
            for cluster in &node.clusters {
                let start = state.data_start + (u32::from(*cluster) - 2) * next.cluster_sectors;
                if let Some((&previous_start, previous)) = extents.last_key_value()
                    && previous_start + previous.sectors == start
                    && previous.path == path
                    && previous.offset + u64::from(previous.sectors) * SECTOR as u64 == offset
                {
                    extents.last_entry().unwrap().get_mut().sectors += next.cluster_sectors;
                } else {
                    extents.insert(
                        start,
                        Extent {
                            path: path.clone(),
                            offset,
                            sectors: next.cluster_sectors,
                        },
                    );
                }
                for sector in start..start + next.cluster_sectors {
                    let count = u64::from(node.size)
                        .saturating_sub(offset)
                        .min(SECTOR as u64) as usize;
                    if count > 0
                        && let Some(bytes) = state.pending.get(&sector).map(|bytes| **bytes)
                    {
                        let output = match &mut file {
                            Some(output) => output,
                            None => file.insert(OpenOptions::new().write(true).open(&path)?),
                        };
                        output.seek(SeekFrom::Start(offset))?;
                        output.write_all(&bytes[..count])?;
                        if count == SECTOR {
                            state.pending.remove(&sector);
                        }
                        self.dirty_files.insert(path.clone());
                    }
                    offset += SECTOR as u64;
                }
            }
        }
        let renamed: BTreeSet<_> = renames.values().collect();
        let mut removed: Vec<_> = self
            .catalog
            .entries
            .iter()
            .filter(|(path, _)| !next.entries.contains_key(*path))
            .collect();
        removed.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
        for (relative, node) in removed {
            let path = self.checked_path(relative)?;
            if !path.try_exists()? || renamed.contains(relative) {
                continue;
            }
            if node.directory {
                fs::remove_dir(&path)?;
            } else {
                fs::remove_file(&path)?;
            }
            self.dirty_files.remove(&path);
        }
        for sector in &next.sectors {
            if let Some(bytes) = state.pending.remove(sector) {
                state.metadata.insert(*sector, bytes);
            }
        }
        state.files = extents;
        self.catalog = next;
        self.metadata_dirty = false;
        Ok(())
    }

    pub fn flush(&mut self) -> io::Result<()> {
        if self.metadata_dirty {
            self.reconcile()?;
        }
        for path in &self.dirty_files {
            if path.try_exists()? {
                OpenOptions::new().write(true).open(path)?.sync_data()?;
            }
        }
        self.dirty_files.clear();
        Ok(())
    }
}
