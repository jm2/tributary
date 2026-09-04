//! Admission: default-deny licence gating and duplicate/byte-hint refusals.

use super::*;

// -- admission ---------------------------------------------------------

#[test]
fn admission_is_default_deny() {
    let (mut engine, _dir) = engine(FakeServer::serving(payload(16, 0)), QUOTA);
    let media = key(source(1), "track-1");
    // Undeclared: refused before any network work.
    assert_eq!(
        engine.admit(media.clone(), labels("t"), None),
        Err(AdmissionRefusal::LicenceDenied)
    );
    // Explicitly unsupported: structurally distinct.
    engine.set_source_position(source(1), SourceOfflinePosition::Unsupported);
    assert_eq!(
        engine.admit(media.clone(), labels("t"), None),
        Err(AdmissionRefusal::UnsupportedSource)
    );
    // Denied and revoked licences refuse; declared admits.
    engine.set_source_position(
        source(1),
        SourceOfflinePosition::Declared(OperationalLicence::Denied),
    );
    assert_eq!(
        engine.admit(media.clone(), labels("t"), None),
        Err(AdmissionRefusal::LicenceDenied)
    );
    engine.set_source_position(
        source(1),
        SourceOfflinePosition::Declared(OperationalLicence::Revoked),
    );
    assert_eq!(
        engine.admit(media.clone(), labels("t"), None),
        Err(AdmissionRefusal::LicenceDenied)
    );
    declared(&mut engine, source(1));
    assert_eq!(engine.admit(media.clone(), labels("t"), None), Ok(()));
}

#[test]
fn admission_refuses_in_flight_duplicates_and_untrusted_byte_hints() {
    let (mut engine, _dir) = engine(FakeServer::serving(payload(16, 0)), QUOTA);
    let media = key(source(1), "track-1");
    declared(&mut engine, source(1));
    assert_eq!(engine.admit(media.clone(), labels("t"), None), Ok(()));
    // One job per media key: the newer request waits.
    assert_eq!(
        engine.admit(media.clone(), labels("t"), None),
        Err(AdmissionRefusal::AlreadyInFlight)
    );
    // A hint above the contract's trusted ceiling is malformed input.
    assert_eq!(
        engine.admit(key(source(1), "track-2"), labels("t"), Some(5 * 1024)),
        Err(AdmissionRefusal::ByteHintUntrusted)
    );
    // After the predecessor reaches a terminal state, a fresh job is
    // admitted cleanly.
    engine.cancel(&media);
    assert_eq!(engine.admit(media, labels("t"), None), Ok(()));
}
