//! Content sources: Steam archives and loose files, independent of gameplay.
//!
//! Discovery labels installation locations, not license entitlements. In particular,
//! the local demo and full release have been observed to contain identical archives.
use crate::archive::Archive;
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

const MAX_RESOURCE: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    LocalArchive,
    SteamFull,
    SteamDemo,
}

#[derive(Debug, Clone)]
pub struct Source {
    pub kind: SourceKind,
    pub path: PathBuf,
}

impl SourceKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::LocalArchive => "Local archive",
            Self::SteamFull => "Steam full installation",
            Self::SteamDemo => "Steam demo installation",
        }
    }
}

/// Search common Steam locations and any libraries listed by Steam. An explicit
/// user-supplied path should be offered as well; discovery is best-effort only.
pub fn discover() -> Vec<Source> {
    let cwd = std::env::current_dir().unwrap_or_default();
    discover_from(&cwd, &steam_roots())
}

fn discover_from(cwd: &Path, roots: &[PathBuf]) -> Vec<Source> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let local = cwd.join("assets/csm.bin");
    if local.is_file() {
        seen.insert(local.canonicalize().unwrap_or_else(|_| local.clone()));
        out.push(Source {
            kind: SourceKind::LocalArchive,
            path: local,
        });
    }
    for root in roots {
        let mut libraries = vec![root.clone()];
        if let Ok(text) = fs::read_to_string(root.join("steamapps/libraryfolders.vdf")) {
            libraries.extend(vdf_values(&text, "path").into_iter().map(PathBuf::from));
        }
        for library in libraries {
            for (id, kind, fallback) in [
                ("2061230", SourceKind::SteamFull, "Chasm The Rift"),
                ("2141610", SourceKind::SteamDemo, "Chasm The Rift Demo"),
            ] {
                let steamapps = library.join("steamapps");
                let manifest = steamapps.join(format!("appmanifest_{id}.acf"));
                // Only consider installed applications. Do not trust the manifest's
                // install directory as an arbitrary path outside steamapps/common.
                let Ok(manifest_text) = fs::read_to_string(manifest) else {
                    continue;
                };
                let install = vdf_values(&manifest_text, "installdir")
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| fallback.to_owned());
                let path = Path::new(&install);
                if path.components().count() != 1
                    || !matches!(path.components().next(), Some(Component::Normal(_)))
                {
                    continue;
                }
                let candidate = steamapps.join("common").join(path).join("csm.bin");
                if candidate.is_file()
                    && seen.insert(
                        candidate
                            .canonicalize()
                            .unwrap_or_else(|_| candidate.clone()),
                    )
                {
                    out.push(Source {
                        kind,
                        path: candidate,
                    });
                }
            }
        }
    }
    out
}

fn steam_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        roots.push(home.join(".local/share/Steam"));
        roots.push(home.join(".steam/steam"));
        roots.push(home.join("Library/Application Support/Steam"));
    }
    for var in ["PROGRAMFILES(X86)", "PROGRAMFILES"] {
        if let Some(root) = std::env::var_os(var) {
            roots.push(PathBuf::from(root).join("Steam"));
        }
    }
    roots
}

/// Read quoted key/value pairs from Valve's text VDF. Scope is irrelevant for
/// the keys we need in the current libraryfolders layout.
fn vdf_values(text: &str, wanted: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' {
            let mut token = String::new();
            while let Some(c) = chars.next() {
                match c {
                    '"' => break,
                    '\\' => match chars.peek().copied() {
                        Some('\\' | '"') => token.push(chars.next().unwrap()),
                        _ => token.push('\\'),
                    },
                    _ => token.push(c),
                }
            }
            tokens.push(Some(token));
        } else if c == '{' || c == '}' {
            tokens.push(None);
        } else if c == '/' && chars.peek() == Some(&'/') {
            for c in chars.by_ref() {
                if c == '\n' {
                    break;
                }
            }
        }
    }
    tokens
        .windows(2)
        .filter_map(|pair| match (&pair[0], &pair[1]) {
            (Some(k), Some(v)) if k.eq_ignore_ascii_case(wanted) => Some(v.clone()),
            _ => None,
        })
        .collect()
}

/// Read a game-owned tar archive or an independently authored loose-file tree.
/// Paths are portable forward-slash resource names and cannot escape the root.
pub enum ResourceSet {
    Archive(Archive),
    Directory {
        root: PathBuf,
        entries: BTreeMap<String, PathBuf>,
    },
}

impl ResourceSet {
    pub fn open(path: &Path) -> io::Result<Self> {
        if path.is_dir() {
            let root = path.canonicalize()?;
            let mut entries = BTreeMap::new();
            index_dir(&root, &root, &mut entries)?;
            Ok(Self::Directory { root, entries })
        } else {
            Archive::open(path).map(Self::Archive)
        }
    }

    pub fn names(&self) -> Box<dyn Iterator<Item = &str> + '_> {
        match self {
            Self::Archive(archive) => Box::new(archive.names()),
            Self::Directory { entries, .. } => Box::new(entries.keys().map(String::as_str)),
        }
    }

    pub fn read(&mut self, name: &str) -> io::Result<Vec<u8>> {
        match self {
            Self::Archive(archive) => archive.read(name),
            Self::Directory { root, entries } => {
                let path = entries.get(name).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("resource not found: {name}"),
                    )
                })?;
                // Re-check the canonical path when opening: entries may have been
                // replaced since indexing, including by symlinks.
                let canonical = path.canonicalize()?;
                if !canonical.starts_with(root.as_path()) {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "resource resolves outside content directory",
                    ));
                }
                let file = File::open(canonical)?;
                if file.metadata()?.len() > MAX_RESOURCE {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "resource exceeds 64 MiB read limit",
                    ));
                }
                let mut data = Vec::new();
                file.take(MAX_RESOURCE + 1).read_to_end(&mut data)?;
                if data.len() as u64 > MAX_RESOURCE {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "resource exceeds 64 MiB read limit",
                    ));
                }
                Ok(data)
            }
        }
    }
}

fn index_dir(root: &Path, dir: &Path, entries: &mut BTreeMap<String, PathBuf>) -> io::Result<()> {
    for item in fs::read_dir(dir)? {
        let item = item?;
        let kind = item.file_type()?;
        let path = item.path();
        if kind.is_dir() {
            index_dir(root, &path, entries)?;
        } else if kind.is_file() {
            let relative = path.strip_prefix(root).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "content path escaped root")
            })?;
            let components: Option<Vec<&str>> = relative
                .components()
                .map(|c| match c {
                    Component::Normal(s) => s.to_str(),
                    _ => None,
                })
                .collect();
            let name = components
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 content path")
                })?
                .join("/");
            entries.insert(name, path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp() -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("openchasm-content-{}-{n}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn reads_valve_vdf_and_escaped_paths() {
        let text = r#""libraryfolders" { "0" { "path" "C:\\Program Files\\Steam" } "1" { "path" "/games/Steam" } }"#;
        assert_eq!(
            vdf_values(text, "path"),
            vec![r"C:\Program Files\Steam", "/games/Steam"]
        );
        assert!(vdf_values("\"path\" { \"something\" \"else\" }", "path").is_empty());
    }

    #[test]
    fn discovers_distinct_full_and_demo_libraries() {
        let temp = temp();
        let root = temp.join("steam");
        let library = temp.join("second library");
        fs::create_dir_all(root.join("steamapps")).unwrap();
        fs::write(
            root.join("steamapps/libraryfolders.vdf"),
            format!(
                "\"libraryfolders\" {{ \"1\" {{ \"path\" \"{}\" }} }}",
                library.display()
            ),
        )
        .unwrap();
        let apps = library.join("steamapps");
        for (id, dir) in [("2061230", "Full"), ("2141610", "Demo")] {
            fs::create_dir_all(apps.join("common").join(dir)).unwrap();
            fs::write(apps.join("common").join(dir).join("csm.bin"), []).unwrap();
            fs::write(
                apps.join(format!("appmanifest_{id}.acf")),
                format!("\"AppState\" {{ \"installdir\" \"{dir}\" }}"),
            )
            .unwrap();
        }
        let found = discover_from(&temp, &[root]);
        assert_eq!(
            found.iter().map(|s| s.kind).collect::<Vec<_>>(),
            vec![SourceKind::SteamFull, SourceKind::SteamDemo]
        );
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn custom_directory_reads_indexed_resources() {
        let temp = temp();
        fs::create_dir_all(temp.join("map")).unwrap();
        fs::write(temp.join("map/resource.01"), b"example").unwrap();
        let mut content = ResourceSet::open(&temp).unwrap();
        assert_eq!(content.names().collect::<Vec<_>>(), vec!["map/resource.01"]);
        assert_eq!(content.read("map/resource.01").unwrap(), b"example");
        assert_eq!(
            content.read("../map/resource.01").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        fs::remove_dir_all(temp).unwrap();
    }
}
