//! Bounded identities for exactly the resources observed during level loading.
use openchasm::save_format::{ContentDigest, ContentManifest};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

const MAX_PATH: usize = 1024;
const MAX_ENTRIES: usize = 4096;
const MAX_READS: usize = 16384;
const MAX_HASHED_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_UNIQUE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_METADATA_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceError {
    AlreadyStarted,
    NotStarted,
    InvalidName,
    LimitExceeded,
    ResourceChanged,
    ResourceIo,
    HashCollision,
    UnrecordedResource,
}
impl std::fmt::Display for TraceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for TraceError {}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Missing,
    Present { length: u64, digest: [u8; 32] },
}
#[derive(Debug, Clone)]
struct Record {
    name: String,
    outcome: Outcome,
}

/// A trace is an identity ledger, never a copy of the resource bytes. Callers
/// add canonical rules/configuration metadata before finalizing the digest.
#[derive(Debug, Default)]
pub struct ResourceTrace {
    entries: BTreeMap<String, Record>,
    reads: usize,
    hashed_bytes: u64,
    unique_bytes: u64,
    failure: Option<TraceError>,
}
impl ResourceTrace {
    fn record(
        &mut self,
        namespace: &str,
        name: &str,
        data: Option<&[u8]>,
    ) -> Result<(), TraceError> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.record_checked(namespace, name, data);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn record_checked(
        &mut self,
        namespace: &str,
        name: &str,
        data: Option<&[u8]>,
    ) -> Result<(), TraceError> {
        if name.is_empty() || name.len() > MAX_PATH || name.contains('\0') {
            return Err(TraceError::InvalidName);
        }
        self.reads = self
            .reads
            .checked_add(1)
            .filter(|n| *n <= MAX_READS)
            .ok_or(TraceError::LimitExceeded)?;
        let len = data.map_or(0, |v| v.len() as u64);
        self.hashed_bytes = self
            .hashed_bytes
            .checked_add(len)
            .filter(|n| *n <= MAX_HASHED_BYTES)
            .ok_or(TraceError::LimitExceeded)?;
        let mut path_hash = Sha256::new();
        path_hash.update(b"OpenChasm resource name v1\0");
        path_hash.update(namespace.as_bytes());
        path_hash.update((name.len() as u64).to_le_bytes());
        path_hash.update(name.as_bytes());
        let name_digest = path_hash.finalize();
        let mut key = format!("{namespace}/");
        for byte in name_digest {
            use std::fmt::Write;
            write!(&mut key, "{byte:02x}").expect("writing to String");
        }
        let outcome = match data {
            Some(data) => Outcome::Present {
                length: len,
                digest: Sha256::digest(data).into(),
            },
            None => Outcome::Missing,
        };
        if let Some(previous) = self.entries.get(&key) {
            if previous.name != name {
                return Err(TraceError::HashCollision);
            }
            if previous.outcome != outcome {
                return Err(TraceError::ResourceChanged);
            }
            return Ok(());
        }
        if self.entries.len() >= MAX_ENTRIES {
            return Err(TraceError::LimitExceeded);
        }
        self.unique_bytes = self
            .unique_bytes
            .checked_add(len)
            .filter(|n| *n <= MAX_UNIQUE_BYTES)
            .ok_or(TraceError::LimitExceeded)?;
        self.entries.insert(
            key,
            Record {
                name: name.to_owned(),
                outcome,
            },
        );
        Ok(())
    }
    pub(crate) fn record_read(
        &mut self,
        name: &str,
        result: &std::io::Result<Vec<u8>>,
        absent: bool,
    ) {
        if self.failure.is_some() {
            return;
        }
        match result {
            Ok(bytes) => {
                let _ = self.record("resource", name, Some(bytes));
            }
            Err(error) if absent && error.kind() == std::io::ErrorKind::NotFound => {
                let _ = self.record("resource", name, None);
            }
            Err(_) => self.failure = Some(TraceError::ResourceIo),
        }
    }
    /// Include a resource already loaded before tracing began. It must be the
    /// exact bytes used in construction; later reads of the name must match.
    pub fn record_preloaded(&mut self, name: &str, bytes: &[u8]) -> Result<(), TraceError> {
        self.record("resource", name, Some(bytes))
    }
    /// Bind caller-defined canonical rules and capability decisions. Labels are
    /// exact UTF-8, domain-separated from resource paths; do not include assets.
    pub fn add_metadata(&mut self, label: &str, bytes: &[u8]) -> Result<(), TraceError> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if bytes.len() > MAX_METADATA_BYTES {
            self.failure = Some(TraceError::LimitExceeded);
            return Err(TraceError::LimitExceeded);
        }
        self.record("metadata", label, Some(bytes))
    }
    /// Freeze exactly the successful startup trace for later checked reads.
    /// The ledger retains names and hashes only, never resource bytes.
    pub fn finish_verified(self) -> Result<VerifiedResources, TraceError> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let mut manifest = ContentManifest::new();
        let mut records = BTreeMap::new();
        for (key, record) in self.entries {
            match record.outcome {
                Outcome::Missing => manifest.add_missing(&key),
                Outcome::Present { length, digest } => manifest.add_digest(&key, length, digest),
            }
            .map_err(|_| TraceError::LimitExceeded)?;
            if key.starts_with("resource/") {
                records.insert(record.name, record.outcome);
            }
        }
        Ok(VerifiedResources {
            digest: manifest.finish(),
            records,
        })
    }
    pub fn finish(self) -> Result<ContentDigest, TraceError> {
        Ok(self.finish_verified()?.digest)
    }
}

/// Immutable successful startup ledger. Keep it only in memory and compare
/// `digest()` with the saved content digest before allowing a restore.
#[derive(Clone, Debug)]
pub struct VerifiedResources {
    digest: ContentDigest,
    records: BTreeMap<String, Outcome>,
}
impl VerifiedResources {
    pub fn digest(&self) -> ContentDigest {
        self.digest
    }
}

pub(crate) struct ReadVerification {
    ledger: VerifiedResources,
    reads: usize,
    hashed_bytes: u64,
    failure: Option<TraceError>,
}
impl ReadVerification {
    pub(crate) fn new(ledger: &VerifiedResources) -> Self {
        Self {
            ledger: ledger.clone(),
            reads: 0,
            hashed_bytes: 0,
            failure: None,
        }
    }
    pub(crate) fn check_name(&mut self, name: &str) -> Result<(), TraceError> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = if name.is_empty() || name.len() > MAX_PATH || name.contains('\0') {
            Err(TraceError::InvalidName)
        } else if self.reads >= MAX_READS {
            Err(TraceError::LimitExceeded)
        } else if !self.ledger.records.contains_key(name) {
            Err(TraceError::UnrecordedResource)
        } else {
            self.reads += 1;
            Ok(())
        };
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub(crate) fn check_result(
        &mut self,
        name: &str,
        result: &std::io::Result<Vec<u8>>,
        absent: bool,
    ) -> Result<(), TraceError> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let expected = self
            .ledger
            .records
            .get(name)
            .ok_or(TraceError::UnrecordedResource)?;
        let outcome = match result {
            Ok(bytes) => {
                let len = bytes.len() as u64;
                match self
                    .hashed_bytes
                    .checked_add(len)
                    .filter(|n| *n <= MAX_HASHED_BYTES)
                {
                    Some(total) => self.hashed_bytes = total,
                    None => {
                        self.failure = Some(TraceError::LimitExceeded);
                        return Err(TraceError::LimitExceeded);
                    }
                }
                Outcome::Present {
                    length: len,
                    digest: Sha256::digest(bytes).into(),
                }
            }
            Err(error) if absent && error.kind() == std::io::ErrorKind::NotFound => {
                Outcome::Missing
            }
            Err(_) => {
                self.failure = Some(TraceError::ResourceIo);
                return Err(TraceError::ResourceIo);
            }
        };
        if &outcome != expected {
            self.failure = Some(TraceError::ResourceChanged);
            return Err(TraceError::ResourceChanged);
        }
        Ok(())
    }
    pub(crate) fn finish(self) -> Result<(), TraceError> {
        self.failure.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn limits_and_failures_are_sticky() {
        let mut trace = ResourceTrace {
            reads: MAX_READS,
            ..Default::default()
        };
        assert_eq!(
            trace.add_metadata("v", b"x"),
            Err(TraceError::LimitExceeded)
        );
        assert_eq!(trace.finish(), Err(TraceError::LimitExceeded));
        let mut trace = ResourceTrace::default();
        assert_eq!(
            trace.add_metadata(&"a".repeat(MAX_PATH + 1), b"x"),
            Err(TraceError::InvalidName)
        );
        assert_eq!(trace.finish(), Err(TraceError::InvalidName));
        let mut trace = ResourceTrace {
            unique_bytes: MAX_UNIQUE_BYTES,
            ..Default::default()
        };
        assert_eq!(
            trace.add_metadata("v", b"x"),
            Err(TraceError::LimitExceeded)
        );
    }
    #[test]
    fn verification_limits_are_sticky_and_do_not_read_unrecorded_names() {
        let mut trace = ResourceTrace::default();
        trace.record_preloaded("path", b"content").unwrap();
        let ledger = trace.finish_verified().unwrap();
        let mut verification = ReadVerification::new(&ledger);
        verification.reads = MAX_READS;
        assert_eq!(
            verification.check_name("path"),
            Err(TraceError::LimitExceeded)
        );
        assert_eq!(verification.finish(), Err(TraceError::LimitExceeded));
        let mut verification = ReadVerification::new(&ledger);
        verification.hashed_bytes = MAX_HASHED_BYTES;
        verification.check_name("path").unwrap();
        assert_eq!(
            verification.check_result("path", &Ok(vec![1]), false),
            Err(TraceError::LimitExceeded)
        );
        assert_eq!(verification.finish(), Err(TraceError::LimitExceeded));
        let mut verification = ReadVerification::new(&ledger);
        assert_eq!(
            verification.check_name("PATH"),
            Err(TraceError::UnrecordedResource)
        );
        assert_eq!(verification.finish(), Err(TraceError::UnrecordedResource));
    }
}
