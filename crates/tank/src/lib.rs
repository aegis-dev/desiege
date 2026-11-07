//! Reader for Dungeon Siege Tank archives (.dsres / .dsmap).
//!
//! Layout (all little-endian):
//! - header: "DSig" "Tank" version, dirset_offset, fileset_offset, index_size, data_offset, ...
//!   The index (dirset + fileset) spans `dirset_offset .. dirset_offset + index_size`.
//! - dirset / fileset: u32 count, then `count` u32 offsets (relative to the set's start).
//! - dir entry:  parent u32, child_count u32, filetime u64, name
//! - file entry: parent u32, size u32, offset u32 (from data_offset), crc u32, filetime u64,
//!   format u16 (0 raw, 1 zlib, 2 lzo), flags u16, name, then for compressed files:
//!   compressed_size u32, chunk_size u32, and one chunk header per `chunk_size` of output:
//!   uncompressed u32, compressed u32, extra u32, offset u32 (from the file's data start).
//!   A chunk is `compressed` zlib bytes followed by `extra` raw bytes that complete it.
//! - name: u16 length, bytes, NUL, padded to 4 bytes.
//!
//! Only the index is kept in memory; file data is read on demand.

use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Error, ErrorKind},
    os::windows::fs::FileExt,
    path::Path,
};

pub struct Tank {
    file: File,
    data_offset: u64,
    index: Vec<u8>,
    /// Lowercase path ("ui/interfaces/backend/data_bar/data_bar.gas") -> entry.
    entries: BTreeMap<String, Entry>,
}

#[derive(Clone, Copy)]
struct Entry {
    size: u32,
    offset: u32,
    format: u16,
    /// Position in `index` right after the name (where the chunk table starts).
    tail: usize,
}

fn bad(msg: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidData, msg.into())
}

/// Bounds-checked little-endian reads from the index buffer.
struct Index<'a>(&'a [u8]);
impl Index<'_> {
    fn u32(&self, at: usize) -> io::Result<u32> {
        let b = self.0.get(at..at + 4).ok_or_else(|| bad("index truncated"))?;
        Ok(u32::from_le_bytes(b.try_into().unwrap()))
    }
    fn u16(&self, at: usize) -> io::Result<u16> {
        let b = self.0.get(at..at + 2).ok_or_else(|| bad("index truncated"))?;
        Ok(u16::from_le_bytes(b.try_into().unwrap()))
    }
    /// Returns (name, position after the padded name).
    fn name(&self, at: usize) -> io::Result<(String, usize)> {
        let len = self.u16(at)? as usize;
        let bytes = self.0.get(at + 2..at + 2 + len).ok_or_else(|| bad("name truncated"))?;
        Ok((bytes.iter().map(|&b| b as char).collect(), (at + 2 + len + 1 + 3) & !3))
    }
}

impl Tank {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Tank> {
        let file = File::open(path)?;
        let mut header = [0u8; 28];
        file.seek_read(&mut header, 0)?;
        if &header[0..8] != b"DSigTank" {
            return Err(bad("not a Tank file"));
        }
        let h = |i: usize| u32::from_le_bytes(header[i..i + 4].try_into().unwrap()) as usize;
        let (dirset, fileset, index_size, data_offset) = (h(12), h(16), h(20), h(24) as u64);
        if fileset < dirset {
            return Err(bad("fileset before dirset"));
        }

        let mut index = vec![0u8; index_size];
        file.seek_read(&mut index, dirset as u64)?;
        let ix = Index(&index);
        let (dir_base, file_base) = (0, fileset - dirset);

        // Directories: offset (relative to dirset) -> (parent offset, name).
        let mut dirs = BTreeMap::new();
        for i in 0..ix.u32(dir_base)? as usize {
            let at = dir_base + ix.u32(dir_base + 4 + 4 * i)? as usize;
            let (name, _) = ix.name(at + 16)?;
            dirs.insert(at - dir_base, (ix.u32(at)? as usize, name));
        }
        let dir_path = |mut at: usize| {
            let mut parts = Vec::new();
            // Bounded walk up to the root, in case of a corrupt cycle.
            for _ in 0..dirs.len() {
                let Some((parent, name)) = dirs.get(&at) else { break };
                if !name.is_empty() {
                    parts.push(name.as_str());
                }
                if *parent == at {
                    break;
                }
                at = *parent;
            }
            parts.reverse();
            parts.join("/")
        };

        let mut entries = BTreeMap::new();
        for i in 0..ix.u32(file_base)? as usize {
            let at = file_base + ix.u32(file_base + 4 + 4 * i)? as usize;
            let (name, tail) = ix.name(at + 28)?;
            let dir = dir_path(ix.u32(at)? as usize);
            let path = if dir.is_empty() { name } else { format!("{dir}/{name}") };
            let entry = Entry { size: ix.u32(at + 4)?, offset: ix.u32(at + 8)?, format: ix.u16(at + 24)?, tail };
            entries.insert(path.to_ascii_lowercase(), entry);
        }
        Ok(Tank { file, data_offset, index, entries })
    }

    /// All file paths, lowercase, sorted.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    /// Size of a file once extracted.
    pub fn size(&self, path: &str) -> Option<u32> {
        self.entries.get(&path.to_ascii_lowercase()).map(|e| e.size)
    }

    /// Extract one file (path is case-insensitive, `/`-separated).
    pub fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        let e = *self.entries.get(&path.to_ascii_lowercase()).ok_or_else(|| Error::new(ErrorKind::NotFound, path.to_string()))?;
        let start = self.data_offset + e.offset as u64;
        let raw = |at: u64, len: usize| -> io::Result<Vec<u8>> {
            let mut buf = vec![0u8; len];
            self.file.seek_read(&mut buf, at)?;
            Ok(buf)
        };
        match e.format {
            0 => raw(start, e.size as usize),
            1 => {
                let ix = Index(&self.index);
                let chunk_size = ix.u32(e.tail + 4)? as usize;
                if chunk_size == 0 {
                    return Err(bad(format!("{path}: zero chunk size")));
                }
                let mut out = Vec::with_capacity(e.size as usize);
                for k in 0..(e.size as usize).div_ceil(chunk_size) {
                    let h = e.tail + 8 + 16 * k;
                    let (us, cs, extra, off) = (ix.u32(h)? as usize, ix.u32(h + 4)? as usize, ix.u32(h + 8)? as usize, ix.u32(h + 12)?);
                    let data = raw(start + off as u64, cs + extra)?;
                    if us == cs {
                        out.extend_from_slice(&data[..us]);
                    } else {
                        let z = miniz_oxide::inflate::decompress_to_vec_zlib(&data[..cs])
                            .map_err(|err| bad(format!("{path}: chunk {k}: {err:?}")))?;
                        out.extend_from_slice(&z);
                        out.extend_from_slice(&data[cs..]);
                    }
                }
                if out.len() != e.size as usize {
                    return Err(bad(format!("{path}: got {} bytes, expected {}", out.len(), e.size)));
                }
                Ok(out)
            }
            // ponytail: LZO isn't used by any retail DS1 archive; add it if a mod ships one.
            f => Err(Error::new(ErrorKind::Unsupported, format!("{path}: compression format {f}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Tank;

    /// Reads every file in the game's Logic.dsres (skipped if the game isn't installed).
    /// Override the location with DS_RESOURCES=<...\Dungeon Siege 1\Resources>.
    #[test]
    fn reads_every_file_in_logic_dsres() {
        let dir = std::env::var("DS_RESOURCES")
            .unwrap_or_else(|_| r"C:\Program Files (x86)\Steam\steamapps\common\Dungeon Siege 1\Resources".into());
        let path = std::path::Path::new(&dir).join("Logic.dsres");
        if !path.is_file() {
            eprintln!("skipped: {} not found", path.display());
            return;
        }
        let tank = Tank::open(&path).unwrap();
        let mut count = 0;
        for p in tank.paths() {
            let data = tank.read(p).unwrap_or_else(|e| panic!("{p}: {e}"));
            assert_eq!(data.len() as u32, tank.size(p).unwrap(), "{p}");
            count += 1;
        }
        assert!(count > 1000, "only {count} files");
        let bar = String::from_utf8(tank.read("UI/Interfaces/Backend/Data_Bar/data_bar.gas").unwrap()).unwrap();
        assert!(bar.starts_with("[data_bar]") && bar.contains("intended_resolution_width = 640"));
    }
}
