//! Public immutable reference-handle lifecycle contract.

use xenoteer_sdk::{ElementHandle, ElementRef, SdkError, WindowHandle, WindowRef};

type TestError = Box<dyn std::error::Error + Send + Sync>;

#[test]
fn window_handle_never_retargets_and_relocation_is_explicit() -> Result<(), TestError> {
    let original: WindowRef = serde_json::from_value(serde_json::json!({
        "desktop_id": "20000000-0000-4000-8000-000000000001",
        "desktop_generation": "30000000-0000-4000-8000-000000000001",
        "xid": 4194305,
        "observed_generation": "1",
        "identity_hash": "1111111111111111111111111111111111111111111111111111111111111111"
    }))?;
    let replacement: WindowRef = serde_json::from_value(serde_json::json!({
        "desktop_id": "20000000-0000-4000-8000-000000000001",
        "desktop_generation": "30000000-0000-4000-8000-000000000001",
        "xid": 4194305,
        "observed_generation": "2",
        "identity_hash": "2222222222222222222222222222222222222222222222222222222222222222"
    }))?;
    let handle = WindowHandle::from_reference(original.clone())?;

    assert!(handle.check_current(&original).is_ok());
    assert!(matches!(
        handle.check_current(&replacement),
        Err(SdkError::StaleReference)
    ));
    assert_eq!(handle.reference(), &original);
    let relocated = handle.relocate(replacement.clone())?;
    assert_eq!(handle.reference(), &original);
    assert_eq!(relocated.reference(), &replacement);
    assert!(!handle.same_identity(&relocated));
    assert!(matches!(
        handle.relocate(original.clone()),
        Err(SdkError::InvalidRequest)
    ));
    let mut foreign = replacement.clone();
    foreign.desktop_generation =
        serde_json::from_value(serde_json::json!("30000000-0000-4000-8000-000000000002"))?;
    assert!(matches!(
        handle.relocate(foreign),
        Err(SdkError::InvalidRequest)
    ));
    let mut malformed = original;
    malformed.xid = 0;
    assert!(matches!(
        WindowHandle::from_reference(malformed),
        Err(SdkError::InvalidResponse)
    ));
    Ok(())
}

#[test]
fn element_handle_ignores_revision_but_rejects_identity_reuse() -> Result<(), TestError> {
    let original: ElementRef = serde_json::from_value(serde_json::json!({
        "desktop_id": "20000000-0000-4000-8000-000000000001",
        "desktop_generation": "30000000-0000-4000-8000-000000000001",
        "atspi_generation": "4",
        "application": {
            "desktop_id": "20000000-0000-4000-8000-000000000001",
            "desktop_generation": "30000000-0000-4000-8000-000000000001",
            "atspi_generation": "4",
            "unique_bus_name": ":1.42",
            "root_object_path": "/org/a11y/root",
            "app_instance_generation": "2",
            "identity_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        },
        "object_path": "/org/a11y/button",
        "object_identity_hash": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "cache_sequence": "78"
    }))?;
    let mut later_revision = original.clone();
    later_revision.cache_sequence = 79;
    let mut reused = later_revision.clone();
    reused.atspi_generation = serde_json::from_value(serde_json::json!("5"))?;
    reused.application.atspi_generation = reused.atspi_generation;
    let handle = ElementHandle::from_reference(original.clone())?;

    assert!(handle.check_current(&later_revision).is_ok());
    assert!(matches!(
        handle.check_current(&reused),
        Err(SdkError::StaleReference)
    ));
    assert_eq!(handle.reference(), &original);
    let relocated = handle.relocate(reused)?;
    assert!(!handle.same_identity(&relocated));
    assert!(matches!(
        handle.relocate(original.clone()),
        Err(SdkError::InvalidRequest)
    ));
    let mut foreign = later_revision;
    foreign.desktop_generation =
        serde_json::from_value(serde_json::json!("30000000-0000-4000-8000-000000000002"))?;
    foreign.application.desktop_generation = foreign.desktop_generation;
    assert!(matches!(
        handle.relocate(foreign),
        Err(SdkError::InvalidRequest)
    ));
    Ok(())
}
