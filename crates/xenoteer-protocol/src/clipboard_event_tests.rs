use super::*;
use uuid::Uuid;

fn valid_event() -> ClipboardOwnerChangedEvent {
    ClipboardOwnerChangedEvent {
        desktop_id: DesktopId::from_uuid(Uuid::from_u128(1)),
        desktop_generation: DesktopGeneration::from_uuid(Uuid::from_u128(2)),
        selection: SelectionName::Clipboard,
        revision: 1,
        owned_by_xenoteer: true,
    }
}

#[test]
fn accepts_content_free_scoped_owner_evidence() {
    assert_eq!(valid_event().validate(), Ok(()));
}

#[test]
fn rejects_nil_scope_and_zero_revision() {
    let mut nil = valid_event();
    nil.desktop_id = DesktopId::from_uuid(Uuid::nil());
    assert_eq!(
        nil.validate(),
        Err(ClipboardEventValidationError::NilIdentifier)
    );
    let mut wrapped = valid_event();
    wrapped.revision = 0;
    assert_eq!(
        wrapped.validate(),
        Err(ClipboardEventValidationError::Revision)
    );
}
