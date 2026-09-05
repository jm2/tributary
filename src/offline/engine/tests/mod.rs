//! Engine tests: shared fixtures plus a programmable fake backend.
//!
//! The scenario files drive the full state machine through [`FakeServer`];
//! every contract failure mode from the failure table is reachable by
//! configuration.

use super::*;
use crate::architecture::identity::TrackId;
use crate::architecture::offline::EntityValidator;
use sha2::{Digest, Sha256};

mod admission;
mod integrity;
mod lifecycle;
mod resume_quota;

const QUOTA: u64 = 10 * 1024;

fn source(n: u128) -> SourceId {
    SourceId::from_uuid(uuid::Uuid::from_u128(0xA000_0000 + n))
}

fn key(src: SourceId, track: &str) -> MediaKey {
    MediaKey::new(src, TrackId::new(track).unwrap())
}

fn payload(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|index| (index as u8).wrapping_add(seed))
        .collect()
}

fn sha(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn labels(title: &str) -> OfflineRowLabels {
    OfflineRowLabels {
        title: title.to_string(),
        artist: "Artist".to_string(),
        album: Some("Album".to_string()),
        source_label: "Subsonic — example.com".to_string(),
    }
}

/// A programmable source adapter that drives the full engine state
/// machine deterministically. All failure modes from the contract's
/// failure table are reachable by configuration.
#[allow(
    clippy::struct_excessive_bools,
    reason = "a test fake exposing every independently-triggerable contract failure mode"
)]
struct FakeServer {
    payload: Vec<u8>,
    etag: String,
    advertised_digest: Option<[u8; 32]>,
    fail_at_op: Option<u32>,
    auth_expire_at_op: Option<u32>,
    mutate_at_op: Option<(u32, Vec<u8>)>,
    no_validator: bool,
    reads: Vec<(u64, u64)>,
    double_fetch_digest: Option<[u8; 32]>,
    double_fetch_fails: bool,
    offline_after_open: bool,
    ops: u32,
    expired: bool,
}

impl FakeServer {
    fn serving(payload: Vec<u8>) -> Self {
        Self {
            advertised_digest: Some(sha(&payload)),
            payload,
            etag: "\"v1\"".to_string(),
            fail_at_op: None,
            auth_expire_at_op: None,
            mutate_at_op: None,
            no_validator: false,
            reads: Vec::new(),
            double_fetch_digest: None,
            double_fetch_fails: false,
            offline_after_open: false,
            ops: 0,
            expired: false,
        }
    }

    fn without_advertised_digest(mut self) -> Self {
        self.advertised_digest = None;
        self
    }

    /// Transient network failure on read operation `op` (1-based), so an
    /// earlier segment lands first and the job pauses mid-transfer.
    fn failing_read_at(mut self, op: u32) -> Self {
        self.fail_at_op = Some(op);
        self
    }

    fn auth_expires_at(mut self, op: u32) -> Self {
        self.auth_expire_at_op = Some(op);
        self
    }

    fn swaps_content_at(mut self, op: u32, replacement: Vec<u8>) -> Self {
        self.mutate_at_op = Some((op, replacement));
        self
    }

    fn without_validator(mut self) -> Self {
        self.no_validator = true;
        self
    }

    fn with_wrong_advertised_digest(mut self) -> Self {
        let mut wrong = sha(&self.payload);
        wrong[0] = wrong[0].wrapping_add(1);
        self.advertised_digest = Some(wrong);
        self
    }

    fn with_lying_double_fetch(mut self) -> Self {
        let mut lie = sha(&self.payload);
        lie[31] = lie[31].wrapping_add(1);
        self.double_fetch_digest = Some(lie);
        self
    }

    fn with_failing_double_fetch(mut self) -> Self {
        self.double_fetch_fails = true;
        self
    }

    fn goes_offline_after_open(mut self) -> Self {
        self.offline_after_open = true;
        self
    }
}

impl TransferBackend for FakeServer {
    fn open(&mut self, _key: &MediaKey) -> Result<TransferOpen, OfflineError> {
        if self.expired {
            return Err(OfflineError::AuthExpired);
        }
        let validator = if self.no_validator {
            None
        } else {
            Some(EntityValidator::ETag(self.etag.clone()))
        };
        Ok(TransferOpen {
            resume_validator: validator,
            advertised_digest: self.advertised_digest,
            total_bytes: Some(self.payload.len() as u64),
        })
    }

    fn read_range(
        &mut self,
        _key: &MediaKey,
        validator: Option<&EntityValidator>,
        start: u64,
        len: u64,
    ) -> Result<RangeOutcome, OfflineError> {
        self.ops += 1;
        if self.offline_after_open {
            return Err(OfflineError::Network);
        }
        if self.fail_at_op == Some(self.ops) {
            return Err(OfflineError::Network);
        }
        if let Some(at) = self.auth_expire_at_op {
            if self.ops >= at {
                self.expired = true;
                return Err(OfflineError::AuthExpired);
            }
        }
        if let Some((at, replacement)) = &self.mutate_at_op {
            if self.ops == *at {
                self.payload = replacement.clone();
                self.etag = "\"v2-stale\"".to_string();
            }
        }
        if let Some(EntityValidator::ETag(tag)) = validator {
            if *tag != self.etag {
                // 200/412 on If-Range: the entity changed.
                return Ok(RangeOutcome::EntityChanged);
            }
        }
        self.reads.push((start, len));
        if start >= self.payload.len() as u64 {
            return Ok(RangeOutcome::Partial(Vec::new()));
        }
        let end = (start + len).min(self.payload.len() as u64);
        Ok(RangeOutcome::Partial(
            self.payload[start as usize..end as usize].to_vec(),
        ))
    }

    fn double_fetch(&mut self, _key: &MediaKey) -> Result<[u8; 32], OfflineError> {
        if self.double_fetch_fails {
            return Err(OfflineError::Network);
        }
        Ok(self
            .double_fetch_digest
            .unwrap_or_else(|| sha(&self.payload)))
    }
}

fn engine(server: FakeServer, quota: u64) -> (OfflineEngine<FakeServer>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let engine = OfflineEngine::new(server, CacheStore::new(dir.path()), quota);
    (engine, dir)
}

fn declared(engine: &mut OfflineEngine<FakeServer>, src: SourceId) {
    engine.set_source_position(
        src,
        SourceOfflinePosition::Declared(OperationalLicence::SourceDeclared),
    );
}

fn committed_snapshot(
    engine: &OfflineEngine<FakeServer>,
    media: &MediaKey,
) -> crate::architecture::offline::CommittedSnapshot {
    match engine.catalogue(media) {
        OfflineCatalogueEntry::Cached(snapshot) => snapshot,
        other => panic!("expected cached snapshot, got {other:?}"),
    }
}

fn drive_until_terminal(engine: &mut OfflineEngine<FakeServer>, media: &MediaKey) -> JobState {
    // Generous bound: a full transfer plus restarts and retry pauses
    // stays far under this for every fixture used here.
    for _ in 0..32 {
        if let Some(state) = engine.drive(media) {
            if state.is_terminal() {
                return state;
            }
        }
    }
    panic!("job never reached a terminal state");
}

fn hex_of(digest: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    digest
        .iter()
        .fold(String::with_capacity(digest.len() * 2), |mut out, byte| {
            let _unused = write!(out, "{byte:02x}");
            out
        })
}
