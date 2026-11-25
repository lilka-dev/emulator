use super::*;

#[derive(Clone)]
pub(super) struct Node {
    pub directory: bool,
    pub size: u32,
    pub clusters: Vec<u16>,
}

pub(super) struct Catalog {
    pub entries: BTreeMap<PathBuf, Node>,
    pub sectors: BTreeSet<u32>,
    pub cluster_sectors: u32,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

struct Reader<'a> {
    state: &'a State,
    fat: u32,
    cluster_sectors: u32,
    directories: BTreeSet<u16>,
    catalog: Catalog,
}

impl Reader<'_> {
    fn chain(&self, mut cluster: u16) -> io::Result<Vec<u16>> {
        let mut chain = Vec::new();
        let mut visited = BTreeSet::new();
        while cluster < 0xfff8 {
            if cluster < 2 || cluster == 0xfff7 || !visited.insert(cluster) {
                return Err(invalid("Invalid SD cluster chain"));
            }
            chain.push(cluster);
            let offset = u32::from(cluster) * 2;
            let bytes = self.state.read_sector(self.fat + offset / SECTOR as u32)?;
            let at = offset as usize % SECTOR;
            cluster = u16::from_le_bytes([bytes[at], bytes[at + 1]]);
        }
        Ok(chain)
    }

    fn directory(&mut self, path: &Path, sectors: Vec<u32>, depth: usize) -> io::Result<()> {
        if depth > 32 {
            return Err(invalid("SD directory nesting is too deep"));
        }
        let mut names = BTreeSet::new();
        let mut long_name = Vec::new();
        let mut expected = 0;
        let mut checksum = 0;
        for sector in sectors {
            self.catalog.sectors.insert(sector);
            let bytes = self.state.read_sector(sector)?;
            for entry in bytes.chunks_exact(32) {
                if entry[0] == 0 {
                    return Ok(());
                }
                if entry[0] == 0xe5 {
                    long_name.clear();
                    expected = 0;
                    continue;
                }
                if entry[11] == 0x0f {
                    let sequence = entry[0] & 0x1f;
                    if sequence == 0 || sequence > 20 {
                        return Err(invalid("Invalid SD long filename"));
                    }
                    if entry[0] & 0x40 != 0 {
                        long_name = vec![0xffff; sequence as usize * 13];
                        expected = sequence;
                        checksum = entry[13];
                    }
                    if sequence != expected || entry[13] != checksum {
                        long_name.clear();
                        expected = 0;
                        continue;
                    }
                    let mut at = (sequence as usize - 1) * 13;
                    for range in [1..11, 14..26, 28..32] {
                        for pair in entry[range].chunks_exact(2) {
                            long_name[at] = u16::from_le_bytes([pair[0], pair[1]]);
                            at += 1;
                        }
                    }
                    expected -= 1;
                    continue;
                }
                let actual = entry[..11]
                    .iter()
                    .fold(0u8, |sum, byte| sum.rotate_right(1).wrapping_add(*byte));
                let name = if !long_name.is_empty() && expected == 0 && checksum == actual {
                    let count = long_name
                        .iter()
                        .position(|c| *c == 0 || *c == 0xffff)
                        .unwrap_or(long_name.len());
                    String::from_utf16(&long_name[..count])
                        .map_err(|_| invalid("Invalid UTF-16 SD filename"))?
                } else {
                    let mut base = String::from_utf8_lossy(&entry[..8]).trim_end().to_owned();
                    let mut extension =
                        String::from_utf8_lossy(&entry[8..11]).trim_end().to_owned();
                    if entry[12] & 8 != 0 {
                        base.make_ascii_lowercase();
                    }
                    if entry[12] & 16 != 0 {
                        extension.make_ascii_lowercase();
                    }
                    if !extension.is_empty() {
                        base.push('.');
                        base.push_str(&extension);
                    }
                    base
                };
                long_name.clear();
                expected = 0;
                if entry[11] & 8 != 0 || name == "." || name == ".." {
                    continue;
                }
                if name.is_empty()
                    || name
                        .chars()
                        .any(|c| c.is_control() || "\\/:*?\"<>|".contains(c))
                    || name.ends_with([' ', '.'])
                    || !names.insert(name.to_lowercase())
                {
                    return Err(invalid("Invalid or duplicated SD filename"));
                }
                let directory = entry[11] & 0x10 != 0;
                let first = u16::from_le_bytes([entry[26], entry[27]]);
                let size = u32::from_le_bytes(entry[28..32].try_into().unwrap());
                let clusters = if first == 0 && !directory && size == 0 {
                    Vec::new()
                } else {
                    self.chain(first)?
                };
                if u64::from(size)
                    > clusters.len() as u64 * u64::from(self.cluster_sectors) * SECTOR as u64
                {
                    return Err(invalid("SD file size exceeds its cluster chain"));
                }
                let full = path.join(name);
                if self.catalog.entries.len() >= 10_000 {
                    return Err(invalid("Too many SD files"));
                }
                self.catalog.entries.insert(
                    full.clone(),
                    Node {
                        directory,
                        size,
                        clusters: clusters.clone(),
                    },
                );
                if directory {
                    if !self.directories.insert(first) {
                        return Err(invalid("SD directory cycle"));
                    }
                    let sectors = clusters
                        .into_iter()
                        .flat_map(|cluster| {
                            let start = self.state.data_start
                                + (u32::from(cluster) - 2) * self.cluster_sectors;
                            start..start + self.cluster_sectors
                        })
                        .collect();
                    self.directory(&full, sectors, depth + 1)?;
                }
            }
        }
        Ok(())
    }
}

pub(super) fn read(state: &State) -> io::Result<Catalog> {
    let boot = state.read_sector(PARTITION as u32)?;
    let word = |at| u16::from_le_bytes([boot[at], boot[at + 1]]) as u32;
    let fat = PARTITION as u32 + word(14);
    let root = fat + u32::from(boot[16]) * word(22);
    let root_sectors = (word(17) * 32).div_ceil(SECTOR as u32);
    let cluster_sectors = u32::from(boot[13]);
    if cluster_sectors == 0 || !cluster_sectors.is_power_of_two() {
        return Err(invalid("Invalid SD cluster size"));
    }
    let mut reader = Reader {
        state,
        fat,
        cluster_sectors,
        directories: BTreeSet::new(),
        catalog: Catalog {
            entries: BTreeMap::new(),
            sectors: BTreeSet::new(),
            cluster_sectors,
        },
    };
    reader.directory(Path::new(""), (root..root + root_sectors).collect(), 0)?;
    reader.catalog.sectors.extend(root..root + root_sectors);
    for node in reader
        .catalog
        .entries
        .values()
        .filter(|node| node.directory)
    {
        for cluster in &node.clusters {
            let start = state.data_start + (u32::from(*cluster) - 2) * cluster_sectors;
            reader
                .catalog
                .sectors
                .extend(start..start + cluster_sectors);
        }
    }
    let mut owned = BTreeSet::new();
    for node in reader.catalog.entries.values() {
        if node.clusters.iter().any(|cluster| !owned.insert(*cluster)) {
            return Err(invalid("SD files share an allocated cluster"));
        }
    }
    Ok(reader.catalog)
}
