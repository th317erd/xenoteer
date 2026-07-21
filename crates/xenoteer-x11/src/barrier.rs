//! Same-connection input requests and reply-producing ordering barriers.

use x11rb::connection::RequestConnection;
use x11rb::protocol::xproto::{ConnectionExt as _, MOTION_NOTIFY_EVENT, QueryPointerReply, Window};
use x11rb::protocol::xtest::ConnectionExt as _;

use crate::{Result, X11Error};

fn validate_motion_request(x: i32, y: i32, delay_ms: u32) -> Result<(i16, i16)> {
    let max = xenoteer_protocol::MAX_XTEST_DELAY_MS;
    if delay_ms > max {
        return Err(X11Error::DelayOutOfRange {
            requested: delay_ms,
            max,
        });
    }
    let root_x = i16::try_from(x).map_err(|_| X11Error::CoordinateOutOfRange { x, y })?;
    let root_y = i16::try_from(y).map_err(|_| X11Error::CoordinateOutOfRange { x, y })?;
    Ok((root_x, root_y))
}

fn with_validated_motion<T>(
    x: i32,
    y: i32,
    delay_ms: u32,
    request: impl FnOnce(i16, i16) -> Result<T>,
) -> Result<T> {
    let (root_x, root_y) = validate_motion_request(x, y, delay_ms)?;
    request(root_x, root_y)
}

/// Send `QueryPointer` and await its reply on the supplied connection.
///
/// When called immediately after XTEST requests on the same connection, the
/// reply proves that the server processed those earlier requests in order.
pub fn query_pointer_barrier<C>(connection: &C, root: Window) -> Result<QueryPointerReply>
where
    C: RequestConnection,
{
    connection
        .query_pointer(root)
        .map_err(|error| X11Error::Connection(error.to_string()))?
        .reply()
        .map_err(|error| X11Error::Reply(error.to_string()))
}

/// Send one absolute XTEST motion event and observe it with a same-connection
/// `QueryPointer` barrier.
pub fn fake_absolute_motion<C>(
    connection: &C,
    root: Window,
    x: i32,
    y: i32,
    delay_ms: u32,
) -> Result<QueryPointerReply>
where
    C: RequestConnection,
{
    with_validated_motion(x, y, delay_ms, |root_x, root_y| {
        connection
            .xtest_fake_input(MOTION_NOTIFY_EVENT, 0, delay_ms, root, root_x, root_y, 0)
            .map_err(|error| X11Error::Connection(error.to_string()))?
            .check()
            .map_err(|error| X11Error::Reply(error.to_string()))?;
        query_pointer_barrier(connection, root)
    })
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::{validate_motion_request, with_validated_motion};
    use crate::X11Error;

    #[test]
    fn xtest_delay_accepts_shared_protocol_boundary() {
        assert!(matches!(validate_motion_request(0, 0, 10_000), Ok((0, 0))));
    }

    #[test]
    fn xtest_delay_rejects_values_above_shared_protocol_boundary() {
        assert!(matches!(
            validate_motion_request(0, 0, 10_001),
            Err(X11Error::DelayOutOfRange {
                requested: 10_001,
                max: 10_000
            })
        ));
        assert!(matches!(
            validate_motion_request(0, 0, u32::MAX),
            Err(X11Error::DelayOutOfRange {
                requested: u32::MAX,
                max: 10_000
            })
        ));
    }

    #[test]
    fn delay_validation_precedes_coordinate_conversion_and_x_request_seam() {
        assert!(matches!(
            validate_motion_request(i32::MAX, i32::MAX, 10_001),
            Err(X11Error::DelayOutOfRange { .. })
        ));

        let request_ran = Cell::new(false);
        let result = with_validated_motion(0, 0, 10_001, |_, _| {
            request_ran.set(true);
            Ok(())
        });
        assert!(matches!(result, Err(X11Error::DelayOutOfRange { .. })));
        assert!(!request_ran.get());
    }
}
