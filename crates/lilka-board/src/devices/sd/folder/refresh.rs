use super::*;
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::JoinHandle,
    time::Duration,
};

#[derive(Clone, PartialEq, Eq)]
struct Entry {
    directory: bool,
    size: u64,
}
type Tree = BTreeMap<PathBuf, Entry>;

pub(super) struct Update {
    generation: u64,
    entries: Tree,
}

pub(super) struct Watcher {
    generation: Arc<AtomicU64>,
    updates: Receiver<Update>,
    stop: Sender<()>,
    thread: Option<JoinHandle<()>>,
}

fn read_tree(root: &Path) -> io::Result<Tree> {
    let mut paths = Vec::new();
    scan(root, root, &mut paths, 0)?;
    paths
        .into_iter()
        .map(|(path, directory)| {
            let size = if directory {
                0
            } else {
                fs::metadata(root.join(&path))?.len()
            };
            if size > u64::from(u32::MAX) {
                return Err(io::Error::other("SD file exceeds FAT16 size limit"));
            }
            Ok((path, Entry { directory, size }))
        })
        .collect()
}

impl Watcher {
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn start(root: PathBuf) -> io::Result<Self> {
        let generation = Arc::new(AtomicU64::new(0));
        let counter = generation.clone();
        let (send, updates) = mpsc::sync_channel(1);
        let (stop, stopped) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("sd-folder".into())
            .spawn(move || {
                let mut previous = None;
                let mut last_error = String::new();
                while matches!(
                    stopped.recv_timeout(Duration::from_secs(1)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    let generation = counter.load(Ordering::Acquire);
                    if generation & 1 != 0 {
                        continue;
                    }
                    let entries = match read_tree(&root) {
                        Ok(entries) => entries,
                        Err(error) => {
                            let message = error.to_string();
                            if message != last_error {
                                eprintln!("SD folder: {message}");
                                last_error = message;
                            }
                            continue;
                        }
                    };
                    last_error.clear();
                    if counter.load(Ordering::Acquire) != generation {
                        continue;
                    }
                    if previous
                        .as_ref()
                        .is_some_and(|(old, tree)| *old == generation && *tree == entries)
                    {
                        continue;
                    }
                    let update = Update {
                        generation,
                        entries: entries.clone(),
                    };
                    match send.try_send(update) {
                        Ok(()) => previous = Some((generation, entries)),
                        Err(mpsc::TrySendError::Full(_)) => {}
                        Err(mpsc::TrySendError::Disconnected(_)) => break,
                    }
                }
            })?;
        Ok(Self {
            generation,
            updates,
            stop,
            thread: Some(thread),
        })
    }

    pub(super) fn written(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }
}
impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl FolderCard {
    pub fn poll_changes(&mut self, sector: u32) {
        let Some(watcher) = &self.watcher else { return };
        if let Ok(update) = watcher.updates.try_recv() {
            self.host_update = Some(update);
        }
        if !self.catalog.sectors.contains(&sector)
            || self.metadata_dirty
            || self
                .last_write
                .is_some_and(|at| at.elapsed() < Duration::from_millis(500))
        {
            return;
        }
        let Some(update) = self.host_update.take() else {
            return;
        };
        if update.generation != watcher.generation.load(Ordering::Acquire) {
            return;
        }
        if let Err(error) = self.import_changes(&update.entries) {
            eprintln!("SD refresh: {error}");
        }
    }

    fn import_changes(&mut self, entries: &Tree) -> io::Result<()> {
        let changed = |path: &Path, node: &catalog::Node| {
            entries.get(path).is_none_or(|entry| {
                entry.directory != node.directory || entry.size != u64::from(node.size)
            })
        };
        let added = entries.iter().any(|(path, entry)| {
            self.catalog.entries.get(path).is_none_or(|node| {
                node.directory != entry.directory || u64::from(node.size) != entry.size
            })
        });
        if !added
            && !self
                .catalog
                .entries
                .iter()
                .any(|(path, node)| changed(path, node))
        {
            return Ok(());
        }
        // Build new FAT metadata separately; failed imports leave the live card untouched.
        let state = Arc::new(Mutex::new(self.state.lock().unwrap().clone()));
        state.lock().unwrap().pending.clear();
        state.lock().unwrap().files.retain(|_, extent| {
            let Ok(path) = extent.path.strip_prefix(&self.root) else {
                return false;
            };
            self.catalog
                .entries
                .get(path)
                .is_some_and(|node| !changed(path, node))
        });
        let filesystem = fatfs::FileSystem::new(
            Volume {
                state: state.clone(),
                position: 0,
            },
            fatfs::FsOptions::new(),
        )?;
        {
            let directory = filesystem.root_dir();
            let mut removed: Vec<_> = self
                .catalog
                .entries
                .iter()
                .filter(|(path, node)| {
                    entries
                        .get(*path)
                        .is_none_or(|entry| entry.directory != node.directory)
                })
                .collect();
            removed.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
            for (path, _) in removed {
                directory.remove(
                    path.to_str()
                        .ok_or_else(|| io::Error::other("SD filename is not UTF-8"))?,
                )?;
            }
            for (path, entry) in entries {
                let name = path
                    .to_str()
                    .ok_or_else(|| io::Error::other("SD filename is not UTF-8"))?;
                if entry.directory {
                    directory.create_dir(name)?;
                    continue;
                }
                if self
                    .catalog
                    .entries
                    .get(path)
                    .is_some_and(|node| !node.directory && u64::from(node.size) == entry.size)
                {
                    continue;
                }
                let mut file = directory.create_file(name)?;
                file.truncate()?;
                let mut offset = 0;
                let zeros = [0; 64 * 1024];
                while offset < entry.size {
                    let count = (entry.size - offset).min(zeros.len() as u64) as usize;
                    state.lock().unwrap().importing = Some(Import {
                        path: self.root.join(path),
                        offset,
                    });
                    let result = file.write_all(&zeros[..count]);
                    state.lock().unwrap().importing = None;
                    result?;
                    offset += count as u64;
                }
                file.flush()?;
            }
        }
        filesystem.unmount()?;
        let mut next = state.lock().unwrap();
        let catalog = catalog::read(&next)?;
        next.files.clear();
        for (path, node) in &catalog.entries {
            if node.directory {
                continue;
            }
            let mut previous = None;
            for (index, cluster) in node.clusters.iter().enumerate() {
                let sector = next.data_start + (u32::from(*cluster) - 2) * catalog.cluster_sectors;
                if let Some(start) = previous
                    && next
                        .files
                        .get(&start)
                        .is_some_and(|extent| start + extent.sectors == sector)
                {
                    next.files.get_mut(&start).unwrap().sectors += catalog.cluster_sectors;
                } else {
                    next.files.insert(
                        sector,
                        Extent {
                            path: self.root.join(path),
                            offset: index as u64
                                * u64::from(catalog.cluster_sectors)
                                * SECTOR as u64,
                            sectors: catalog.cluster_sectors,
                        },
                    );
                    previous = Some(sector);
                }
            }
        }
        drop(next);
        self.state = state;
        self.catalog = catalog;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> io::Result<PathBuf> {
        let root = std::env::temp_dir().join(format!(
            "lilka-refresh-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root)?;
        Ok(root)
    }
    fn contents(card: &FolderCard, path: &str) -> io::Result<Vec<u8>> {
        let filesystem = fatfs::FileSystem::new(
            Volume {
                state: card.state.clone(),
                position: 0,
            },
            fatfs::FsOptions::new(),
        )?;
        let mut bytes = Vec::new();
        filesystem
            .root_dir()
            .open_file(path)?
            .read_to_end(&mut bytes)?;
        filesystem.unmount()?;
        Ok(bytes)
    }

    #[test]
    fn host_changes_preserve_unmodified_cluster_chains_and_file_contents() -> io::Result<()> {
        let root = root()?;
        fs::write(root.join("keep.bin"), vec![0xa5; 32_000])?;
        fs::write(root.join("grow.txt"), b"small")?;
        fs::write(root.join("remove.txt"), b"gone")?;
        fs::create_dir(root.join("old"))?;
        fs::write(root.join("old/child"), b"old")?;
        let mut card = FolderCard::open(&root)?;
        let clusters = card.catalog.entries[Path::new("keep.bin")].clusters.clone();
        fs::write(root.join("grow.txt"), vec![b'G'; 35_000])?;
        fs::remove_file(root.join("remove.txt"))?;
        fs::rename(root.join("old"), root.join("new"))?;
        fs::write(root.join("новий.lua"), b"print(42)")?;
        card.import_changes(&read_tree(&root)?)?;
        assert_eq!(
            card.catalog.entries[Path::new("keep.bin")].clusters,
            clusters
        );
        assert_eq!(contents(&card, "keep.bin")?, vec![0xa5; 32_000]);
        assert_eq!(contents(&card, "grow.txt")?, vec![b'G'; 35_000]);
        assert_eq!(contents(&card, "новий.lua")?, b"print(42)");
        assert_eq!(contents(&card, "new/child")?, b"old");
        assert!(!card.catalog.entries.contains_key(Path::new("remove.txt")));
        assert!(!card.catalog.entries.contains_key(Path::new("old")));
        fs::write(root.join("grow.txt"), b"short")?;
        card.import_changes(&read_tree(&root)?)?;
        assert_eq!(contents(&card, "grow.txt")?, b"short");
        card.flush()?;
        assert_eq!(fs::read(root.join("grow.txt"))?, b"short");
        drop(card);
        fs::remove_dir_all(root)
    }

    #[test]
    fn failed_import_does_not_replace_live_metadata() -> io::Result<()> {
        let root = root()?;
        fs::write(root.join("keep"), b"intact")?;
        let mut card = FolderCard::open(&root)?;
        let state = card.state.clone();
        let mut tree = read_tree(&root)?;
        tree.insert(
            PathBuf::from("missing/child"),
            Entry {
                directory: false,
                size: 1,
            },
        );
        assert!(card.import_changes(&tree).is_err());
        assert!(Arc::ptr_eq(&state, &card.state));
        assert_eq!(contents(&card, "keep")?, b"intact");
        drop(card);
        fs::remove_dir_all(root)
    }

    #[test]
    fn watcher_defers_updates_during_guest_writes_and_data_reads() -> io::Result<()> {
        let root = root()?;
        fs::write(root.join("keep"), b"intact")?;
        let mut card = FolderCard::open(&root)?;
        fs::write(root.join("added"), b"hello")?;
        let directory = *card.catalog.sectors.first().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while card.host_update.is_none() && std::time::Instant::now() < deadline {
            card.last_write = Some(std::time::Instant::now());
            card.poll_changes(directory);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            card.host_update.is_some(),
            "folder watcher did not report the change"
        );
        assert!(!card.catalog.entries.contains_key(Path::new("added")));
        card.last_write = None;
        let data_start = card.state.lock().unwrap().data_start;
        card.poll_changes(data_start);
        assert!(!card.catalog.entries.contains_key(Path::new("added")));
        card.poll_changes(directory);
        assert_eq!(contents(&card, "added")?, b"hello");
        drop(card);
        fs::remove_dir_all(root)
    }
}
