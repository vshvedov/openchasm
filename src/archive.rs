//! Read the uncompressed tar resource bundle without extracting its contents.
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

const BLOCK: u64 = 512;
const MAX_RESOURCE: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
struct Entry {
    offset: u64,
    size: u64,
}

pub struct Archive {
    file: File,
    entries: BTreeMap<String, Entry>,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn field(bytes: &[u8]) -> io::Result<&str> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end]).map_err(|_| invalid("non-UTF-8 tar header"))
}
fn octal(bytes: &[u8]) -> io::Result<u64> {
    let value = field(bytes)?.trim();
    if value.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(value, 8).map_err(|_| invalid("invalid tar octal field"))
}

impl Archive {
    pub fn open(path: &Path) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let file_len = file.metadata()?.len();
        let mut entries = BTreeMap::new();
        let mut pos = 0_u64;
        loop {
            if pos
                .checked_add(BLOCK)
                .filter(|&end| end <= file_len)
                .is_none()
            {
                return Err(invalid(
                    "tar archive has no end marker or has a truncated header",
                ));
            }
            file.seek(SeekFrom::Start(pos))?;
            let mut header = [0_u8; 512];
            file.read_exact(&mut header)?;
            if header.iter().all(|&b| b == 0) {
                break;
            }
            let expected = octal(&header[148..156])?;
            let actual: u64 = header
                .iter()
                .enumerate()
                .map(|(i, &b)| {
                    if (148..156).contains(&i) {
                        32
                    } else {
                        u64::from(b)
                    }
                })
                .sum();
            if expected != actual {
                return Err(invalid("tar header checksum mismatch"));
            }
            let name = field(&header[..100])?;
            let prefix = field(&header[345..500])?;
            let path = if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}/{name}")
            };
            let size = octal(&header[124..136])?;
            let offset = pos
                .checked_add(BLOCK)
                .ok_or_else(|| invalid("tar offset overflow"))?;
            let padded = size
                .checked_add(BLOCK - 1)
                .ok_or_else(|| invalid("tar size overflow"))?
                / BLOCK
                * BLOCK;
            pos = offset
                .checked_add(padded)
                .ok_or_else(|| invalid("tar offset overflow"))?;
            if pos > file_len {
                return Err(invalid("truncated tar resource"));
            }
            match header[156] {
                0 | b'0' => {
                    if path.is_empty() {
                        return Err(invalid("empty tar resource name"));
                    }
                    if entries.insert(path, Entry { offset, size }).is_some() {
                        return Err(invalid("duplicate tar resource name"));
                    }
                }
                b'5' => {}
                _ => return Err(invalid("unsupported tar entry type")),
            }
        }
        Ok(Self { file, entries })
    }
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }
    pub fn read(&mut self, name: &str) -> io::Result<Vec<u8>> {
        let entry = self.entries.get(name).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("resource not found: {name}"),
            )
        })?;
        if entry.size > MAX_RESOURCE {
            return Err(invalid("resource exceeds 64 MiB read limit"));
        }
        self.file.seek(SeekFrom::Start(entry.offset))?;
        let mut data = vec![0; entry.size as usize];
        self.file.read_exact(&mut data)?;
        Ok(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_octal_and_rejects_bad_numbers() {
        assert_eq!(octal(b"0000012\0").unwrap(), 10);
        assert!(octal(b"000008\0").is_err());
    }
    #[test]
    fn reads_a_resource_and_rejects_corrupt_headers() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "openchasm-test-{}-{unique}.tar",
            std::process::id()
        ));
        let mut header = [0_u8; 512];
        header[..8].copy_from_slice(b"test.txt");
        header[124..136].copy_from_slice(b"00000000003\0");
        header[148..156].fill(b' ');
        header[156] = b'0';
        let sum: u64 = header.iter().map(|&b| u64::from(b)).sum();
        let checksum = format!("{sum:06o}\0 ");
        header[148..156].copy_from_slice(checksum.as_bytes());
        let mut tar = header.to_vec();
        tar.extend_from_slice(b"abc");
        tar.resize(512 * 3, 0);
        std::fs::write(&path, &tar).unwrap();
        let mut archive = Archive::open(&path).unwrap();
        assert_eq!(archive.names().collect::<Vec<_>>(), vec!["test.txt"]);
        assert_eq!(archive.read("test.txt").unwrap(), b"abc");
        assert_eq!(
            archive.read("missing").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        tar[0] ^= 1;
        std::fs::write(&path, tar).unwrap();
        assert!(Archive::open(&path).is_err());
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn rejects_non_utf8_names() {
        assert!(field(&[255, 0]).is_err());
    }
}
