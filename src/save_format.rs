//! Versioned, bounded in-memory save envelope and deterministic content identity.
//!
//! This module treats the payload as opaque bytes. It does not serialize a
//! Session, access the filesystem, authenticate untrusted data, or embed assets.
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;

pub const MAX_SAVE_PAYLOAD: usize = 16 * 1024 * 1024;
pub const MAX_CONTENT_ENTRIES: usize = 4096;
pub const MAX_RESOURCE_NAME: usize = 255;
const MAGIC: &[u8; 8] = b"OPCHSAVE";
const MAJOR: u16 = 1;
const MINOR: u16 = 0;
const HEADER: usize = 8 + 2 + 2 + 32 + 4;
const CHECKSUM: usize = 32;
const CONTENT_DOMAIN: &[u8] = b"OpenChasm content manifest v1\0";

/// Save-format or content-manifest validation error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveError {
    PayloadTooLarge,
    Truncated,
    BadMagic,
    UnsupportedVersion { major: u16, minor: u16 },
    LengthMismatch,
    ChecksumMismatch,
    ContentMismatch,
    InvalidResourceName,
    DuplicateResource,
    TooManyResources,
}
impl fmt::Display for SaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for SaveError {}

/// A SHA-256 content identity. The hash is not an entitlement or authenticity proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContentDigest(pub [u8; 32]);

/// Borrowed validated payload; caller must independently validate its schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SaveEnvelope<'a> {
    pub content: ContentDigest,
    pub payload: &'a [u8],
}

/// Serialize an opaque payload with a version, content digest and integrity hash.
pub fn encode(content: ContentDigest, payload: &[u8]) -> Result<Vec<u8>, SaveError> {
    if payload.len() > MAX_SAVE_PAYLOAD {
        return Err(SaveError::PayloadTooLarge);
    }
    let mut out = Vec::with_capacity(HEADER + payload.len() + CHECKSUM);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&MAJOR.to_le_bytes());
    out.extend_from_slice(&MINOR.to_le_bytes());
    out.extend_from_slice(&content.0);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out.extend_from_slice(&Sha256::digest(&out));
    Ok(out)
}

/// Decode with a strict size cap and exact consumption; no allocation is needed.
pub fn decode(bytes: &[u8]) -> Result<SaveEnvelope<'_>, SaveError> {
    if bytes.len() < HEADER + CHECKSUM {
        return Err(SaveError::Truncated);
    }
    if &bytes[..8] != MAGIC {
        return Err(SaveError::BadMagic);
    }
    let major = u16::from_le_bytes([bytes[8], bytes[9]]);
    let minor = u16::from_le_bytes([bytes[10], bytes[11]]);
    if (major, minor) != (MAJOR, MINOR) {
        return Err(SaveError::UnsupportedVersion { major, minor });
    }
    let length = u32::from_le_bytes(bytes[44..48].try_into().expect("fixed header")) as usize;
    if length > MAX_SAVE_PAYLOAD {
        return Err(SaveError::PayloadTooLarge);
    }
    let total = HEADER + length + CHECKSUM;
    if bytes.len() < total {
        return Err(SaveError::Truncated);
    }
    if bytes.len() != total {
        return Err(SaveError::LengthMismatch);
    }
    let expected = Sha256::digest(&bytes[..HEADER + length]);
    if expected.as_slice() != &bytes[HEADER + length..] {
        return Err(SaveError::ChecksumMismatch);
    }
    let content = ContentDigest(bytes[12..44].try_into().expect("fixed header"));
    Ok(SaveEnvelope {
        content,
        payload: &bytes[HEADER..HEADER + length],
    })
}

/// As `decode`, additionally rejecting saves built for other gameplay content.
pub fn decode_for_content(
    bytes: &[u8],
    expected: ContentDigest,
) -> Result<SaveEnvelope<'_>, SaveError> {
    let save = decode(bytes)?;
    if save.content != expected {
        return Err(SaveError::ContentMismatch);
    }
    Ok(save)
}

#[derive(Debug, Clone, Copy)]
enum Resource {
    Missing,
    Present { length: u64, sha256: [u8; 32] },
}

/// Hashes an order-independent, explicitly resolved resource manifest. Names
/// must already be canonical lower-case ASCII relative resource identifiers.
/// Record optional missing resources so fallback choices affect identity.
#[derive(Debug, Default)]
pub struct ContentManifest {
    entries: BTreeMap<String, Resource>,
}
impl ContentManifest {
    pub fn new() -> Self {
        Self::default()
    }
    fn insert(&mut self, name: &str, resource: Resource) -> Result<(), SaveError> {
        if name.is_empty()
            || name.len() > MAX_RESOURCE_NAME
            || !name.bytes().all(|b| {
                b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || matches!(b, b'/' | b'.' | b'_' | b'-')
            })
            || name
                .split('/')
                .any(|s| s.is_empty() || s == "." || s == "..")
        {
            return Err(SaveError::InvalidResourceName);
        }
        if self.entries.contains_key(name) {
            return Err(SaveError::DuplicateResource);
        }
        if self.entries.len() >= MAX_CONTENT_ENTRIES {
            return Err(SaveError::TooManyResources);
        }
        self.entries.insert(name.to_owned(), resource);
        Ok(())
    }
    pub fn add_missing(&mut self, name: &str) -> Result<(), SaveError> {
        self.insert(name, Resource::Missing)
    }
    pub fn add_bytes(&mut self, name: &str, bytes: &[u8]) -> Result<(), SaveError> {
        self.add_digest(name, bytes.len() as u64, Sha256::digest(bytes).into())
    }
    /// Accept a digest computed by a streaming reader; callers must hash the
    /// exact resolved bytes and provide their exact byte count.
    pub fn add_digest(
        &mut self,
        name: &str,
        length: u64,
        sha256: [u8; 32],
    ) -> Result<(), SaveError> {
        self.insert(name, Resource::Present { length, sha256 })
    }
    pub fn finish(&self) -> ContentDigest {
        let mut hash = Sha256::new();
        hash.update(CONTENT_DOMAIN);
        hash.update((self.entries.len() as u32).to_le_bytes());
        for (name, resource) in &self.entries {
            hash.update((name.len() as u16).to_le_bytes());
            hash.update(name.as_bytes());
            match resource {
                Resource::Missing => hash.update([0]),
                Resource::Present { length, sha256 } => {
                    hash.update([1]);
                    hash.update(length.to_le_bytes());
                    hash.update(sha256);
                }
            }
        }
        ContentDigest(hash.finalize().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn envelope_round_trips_and_checks_content() {
        let content = ContentDigest([42; 32]);
        for data in [&b""[..], &b"snapshot\0bytes"[..]] {
            let bytes = encode(content, data).unwrap();
            assert_eq!(
                decode(&bytes).unwrap(),
                SaveEnvelope {
                    content,
                    payload: data
                }
            );
            assert_eq!(
                decode_for_content(&bytes, ContentDigest([0; 32])),
                Err(SaveError::ContentMismatch)
            );
        }
    }
    #[test]
    fn rejects_corruption_versions_and_bad_lengths() {
        let bytes = encode(ContentDigest([1; 32]), b"abc").unwrap();
        assert_eq!(decode(&bytes[..20]), Err(SaveError::Truncated));
        let mut bad = bytes.clone();
        bad[0] ^= 1;
        assert_eq!(decode(&bad), Err(SaveError::BadMagic));
        let mut bad = bytes.clone();
        bad[8] = 2;
        assert_eq!(
            decode(&bad),
            Err(SaveError::UnsupportedVersion { major: 2, minor: 0 })
        );
        let mut bad = bytes.clone();
        bad[10] = 1;
        assert_eq!(
            decode(&bad),
            Err(SaveError::UnsupportedVersion { major: 1, minor: 1 })
        );
        let mut bad = bytes.clone();
        bad[HEADER] ^= 1;
        assert_eq!(decode(&bad), Err(SaveError::ChecksumMismatch));
        let mut bad = bytes.clone();
        bad[12] ^= 1;
        assert_eq!(decode(&bad), Err(SaveError::ChecksumMismatch));
        let mut bad = bytes.clone();
        bad.push(0);
        assert_eq!(decode(&bad), Err(SaveError::LengthMismatch));
        let mut bad = bytes.clone();
        bad[44..48].copy_from_slice(&((MAX_SAVE_PAYLOAD as u32) + 1).to_le_bytes());
        assert_eq!(decode(&bad), Err(SaveError::PayloadTooLarge));
        let mut bad = bytes.clone();
        bad[44..48].copy_from_slice(&4u32.to_le_bytes());
        assert_eq!(decode(&bad), Err(SaveError::Truncated));
        assert_eq!(
            encode(ContentDigest([0; 32]), &vec![0; MAX_SAVE_PAYLOAD + 1]),
            Err(SaveError::PayloadTooLarge)
        );
    }
    #[test]
    fn manifest_is_order_independent_and_includes_presence_length_and_name() {
        let mut a = ContentManifest::new();
        a.add_bytes("map/map.01", b"map").unwrap();
        a.add_missing("monster/a.car").unwrap();
        let mut b = ContentManifest::new();
        b.add_missing("monster/a.car").unwrap();
        b.add_bytes("map/map.01", b"map").unwrap();
        assert_eq!(a.finish(), b.finish());
        let mut c = ContentManifest::new();
        c.add_bytes("map/map.01", b"map").unwrap();
        c.add_bytes("monster/a.car", b"").unwrap();
        assert_ne!(a.finish(), c.finish());
        let mut d = ContentManifest::new();
        d.add_digest("map/map.01", 4, Sha256::digest(b"map").into())
            .unwrap();
        d.add_missing("monster/a.car").unwrap();
        assert_ne!(a.finish(), d.finish());
        assert_eq!(
            a.add_missing("map/map.01"),
            Err(SaveError::DuplicateResource)
        );
        for bad in ["", "../x", "a//b", "/x", "a/./b", "UPPER", "a\\b", "a/.."] {
            assert_eq!(a.add_missing(bad), Err(SaveError::InvalidResourceName));
        }
    }
    #[test]
    fn manifest_is_bounded_without_partial_insertion() {
        let mut manifest = ContentManifest::new();
        for i in 0..MAX_CONTENT_ENTRIES {
            manifest.add_missing(&format!("resource/{i}")).unwrap();
        }
        let before = manifest.finish();
        assert_eq!(
            manifest.add_missing("overflow"),
            Err(SaveError::TooManyResources)
        );
        assert_eq!(manifest.finish(), before);
    }
}
