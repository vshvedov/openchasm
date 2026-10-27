//! Versioned, bounded in-memory Session snapshot codec.
//!
//! Decoding is structural only; restore onto a verified fresh Session before use.
use crate::save_format::{self, ContentDigest, SaveError};
use crate::session::SessionSnapshot;
use serde::{Deserialize, Serialize};
use std::io::{self, Write};

/// Smaller than the outer envelope cap to bound JSON parsing and allocation.
pub const MAX_SESSION_PAYLOAD: usize = 4 * 1024 * 1024;
const MAX_ENTRIES: usize = 65_536;
const MAX_MAP_CELLS: usize = 4096;
const SCHEMA_VERSION: u16 = 4;

#[derive(Debug)]
pub enum SessionCodecError {
    Envelope(SaveError),
    PayloadTooLarge,
    InvalidPayload,
    UnsupportedVersion(u16),
    InvalidCardinality,
    NonPortableValue,
}
impl std::fmt::Display for SessionCodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for SessionCodecError {}
impl From<SaveError> for SessionCodecError {
    fn from(error: SaveError) -> Self {
        Self::Envelope(error)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    version: u16,
    snapshot: SessionSnapshot,
}

#[derive(Serialize)]
struct PayloadRef<'a> {
    version: u16,
    snapshot: &'a SessionSnapshot,
}

struct LimitedWriter(Vec<u8>);
impl Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_SESSION_PAYLOAD.saturating_sub(self.0.len()) {
            return Err(io::Error::other("session payload exceeds limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn validate_shape(s: &SessionSnapshot) -> Result<(), SessionCodecError> {
    let invalid = || SessionCodecError::InvalidCardinality;
    let all_bounded = [
        s.world.walls.len(),
        s.processes.wall_identity.len(),
        s.processes.elevations.len(),
        s.processes.objects.len(),
        s.pickups.entries.len(),
        s.wall_elevations.len(),
        s.grid_journal.len(),
        s.projectiles.projectiles.len(),
        s.projectile_heights.len(),
        s.projectile_vertical_speeds.len(),
        s.mines.mines.len(),
    ]
    .into_iter()
    .all(|n| n <= MAX_MAP_CELLS);
    if s.light_sources.as_ref().is_some_and(|lights| {
        lights.active.len() > MAX_ENTRIES
            || lights
                .active
                .iter()
                .any(|entry| entry.sources.len() > MAX_MAP_CELLS)
    }) {
        return Err(invalid());
    }
    if !all_bounded
        || s.processes.processes.len() > MAX_ENTRIES
        || s.actors.actors.len() > MAX_ENTRIES
        || s.ai.brains.len() > MAX_ENTRIES
        || s.hazards.active.len() > MAX_ENTRIES
        || s.wind.active.len() > MAX_ENTRIES
        || s.mission
            .as_ref()
            .is_some_and(|m| m.first_messages_shown.len() > MAX_ENTRIES)
        || s.object_states
            .as_ref()
            .is_some_and(|o| o.entries.len() > MAX_MAP_CELLS)
        || s.object_combat
            .as_ref()
            .is_some_and(|o| o.entries.len() > MAX_MAP_CELLS)
    {
        return Err(invalid());
    }
    if s.elevation_by_cell.len() != MAX_MAP_CELLS
        || s.wind.cells.len() != MAX_MAP_CELLS
        || s.wind.actor_cells.len() != MAX_MAP_CELLS
        || s.ambient
            .as_ref()
            .is_some_and(|a| a.cells.len() != MAX_MAP_CELLS)
        || s.quake
            .as_ref()
            .is_some_and(|q| q.cells.len() != MAX_MAP_CELLS)
        || s.processes
            .cell_types
            .as_ref()
            .is_some_and(|c| c.len() != MAX_MAP_CELLS)
    {
        return Err(invalid());
    }
    if s.processes.processes.iter().any(|p| {
        p.wall_progress.len() > MAX_MAP_CELLS
            || p.object_progress.len() > MAX_MAP_CELLS
            || p.previous_cell_types.len() > MAX_MAP_CELLS
    }) || s
        .hazards
        .active
        .iter()
        .any(|h| h.zones.len() > MAX_MAP_CELLS)
        || s.wind
            .active
            .iter()
            .any(|(_, zones)| zones.len() > MAX_MAP_CELLS)
    {
        return Err(invalid());
    }
    if s.object_states
        .as_ref()
        .is_some_and(|v| v.entries.iter().any(|o| o.cell.iter().any(|c| *c >= 64)))
        || s.object_combat
            .as_ref()
            .is_some_and(|v| v.entries.iter().any(|o| o.cell.iter().any(|c| *c >= 64)))
    {
        return Err(SessionCodecError::NonPortableValue);
    }
    Ok(())
}

/// Encode a structural DTO inside the checked content envelope. This does not
/// establish that the DTO is a reachable or restorable simulation state.
pub fn encode(snapshot: &SessionSnapshot) -> Result<Vec<u8>, SessionCodecError> {
    validate_shape(snapshot)?;
    let mut writer = LimitedWriter(Vec::new());
    serde_json::to_writer(
        &mut writer,
        &PayloadRef {
            version: SCHEMA_VERSION,
            snapshot,
        },
    )
    .map_err(|error| {
        if error.is_io() {
            SessionCodecError::PayloadTooLarge
        } else {
            SessionCodecError::InvalidPayload
        }
    })?;
    // serde_json maps non-finite floats to null. Reject any lossy encoding.
    let decoded: Payload =
        serde_json::from_slice(&writer.0).map_err(|_| SessionCodecError::NonPortableValue)?;
    if decoded.snapshot != *snapshot {
        return Err(SessionCodecError::NonPortableValue);
    }
    Ok(save_format::encode(snapshot.content, &writer.0)?)
}

/// Checks envelope and schema, then returns an untrusted DTO. The caller must
/// validate its semantic consistency using `Session::restore_from_baseline`.
pub fn decode(bytes: &[u8], expected: ContentDigest) -> Result<SessionSnapshot, SessionCodecError> {
    let envelope = save_format::decode_for_content(bytes, expected)?;
    if envelope.payload.len() > MAX_SESSION_PAYLOAD {
        return Err(SessionCodecError::PayloadTooLarge);
    }
    let mut payload: Payload =
        serde_json::from_slice(envelope.payload).map_err(|_| SessionCodecError::InvalidPayload)?;
    if matches!(payload.version, 1 | 2)
        && (payload.snapshot.pitch != 0.0
            || !payload.snapshot.projectile_vertical_speeds.is_empty())
    {
        return Err(SessionCodecError::InvalidPayload);
    }
    match payload.version {
        1 => {
            if payload.snapshot.light_tick != 0
                || payload.snapshot.light_sources.is_some()
                || payload
                    .snapshot
                    .processes
                    .processes
                    .iter()
                    .any(|p| p.source_active)
                || payload.snapshot.grid_journal.iter().any(|entry| {
                    matches!(
                        entry,
                        crate::session::GridJournalEntry::SourceActivate { .. }
                            | crate::session::GridJournalEntry::SourceDeactivate { .. }
                    )
                })
            {
                return Err(SessionCodecError::InvalidPayload);
            }
            if payload.snapshot.capabilities.source_lights {
                payload.snapshot.light_sources =
                    Some(crate::light_sources::LightSourcesSnapshot::default());
            }
        }
        2 | 3 | SCHEMA_VERSION => {}
        version => return Err(SessionCodecError::UnsupportedVersion(version)),
    }
    if payload.snapshot.content != envelope.content {
        return Err(SessionCodecError::Envelope(SaveError::ContentMismatch));
    }
    validate_shape(&payload.snapshot)?;
    Ok(payload.snapshot)
}
