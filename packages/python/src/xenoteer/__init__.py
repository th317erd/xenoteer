# SPDX-License-Identifier: Apache-2.0
"""Async Python SDK for the frozen Xenoteer v1 desktop automation API."""

from .artifacts import (
    MAX_ARTIFACT_BYTES,
    MAX_CLIPBOARD_ARTIFACT_BYTES,
    ArtifactRef,
    Artifacts,
)
from .client import (
    DesktopStatus,
    ProtocolVersion,
    Status,
    XenoteerClient,
    admit_request_version,
    negotiate_protocol,
    validate_status,
)
from .command import (
    CommandHandle,
    CommandSubmission,
    TerminalEffect,
    classify_terminal_effect,
    validate_client_command_envelope,
)
from .desktop import (
    Accessibility,
    Applications,
    Capture,
    Clipboard,
    Desktop,
    Element,
    Viewer,
    ViewerTicket,
    Window,
    Windows,
)
from .errors import XenoteerError
from .events import (
    CloseInfo,
    EventItem,
    EventSession,
    KnownEvent,
    ReplayComplete,
    ResyncRequired,
    ServerDraining,
    ServerError,
    SubscriptionAck,
    UnknownEvent,
    UnknownServerMessage,
    XenoteerEvent,
    decode_event_message,
    decode_server_message,
)
from .lease import (
    ControlContext,
    ControlledClipboard,
    ControlLease,
    Keyboard,
    Mouse,
)
from .options import (
    BearerToken,
    ClientOptions,
    ProtocolRange,
    TokenProvider,
    TokenSource,
)
from .policy import (
    CommandRecoveryPolicy,
    EventContinuityPolicy,
    RecoveryDecision,
    ReferenceLifecycle,
)
from .protocol_generated import CanonicalUInt64, JsonObject, JsonValue
from .transport import AsyncDeadlineTransport, AsyncTransport, HttpTransport
from .wire import (
    UINT64_MAX,
    as_uint64_string,
    decode_uint64,
    encode_uint64,
)


__all__ = [
    "Accessibility",
    "Applications",
    "ArtifactRef",
    "Artifacts",
    "AsyncDeadlineTransport",
    "AsyncTransport",
    "BearerToken",
    "CanonicalUInt64",
    "Capture",
    "CloseInfo",
    "ClientOptions",
    "Clipboard",
    "CommandHandle",
    "CommandRecoveryPolicy",
    "CommandSubmission",
    "ControlContext",
    "ControlLease",
    "ControlledClipboard",
    "Desktop",
    "DesktopStatus",
    "Element",
    "EventItem",
    "EventContinuityPolicy",
    "EventSession",
    "HttpTransport",
    "JsonObject",
    "JsonValue",
    "Keyboard",
    "KnownEvent",
    "MAX_ARTIFACT_BYTES",
    "MAX_CLIPBOARD_ARTIFACT_BYTES",
    "Mouse",
    "ProtocolRange",
    "ProtocolVersion",
    "RecoveryDecision",
    "ReferenceLifecycle",
    "ReplayComplete",
    "ResyncRequired",
    "ServerDraining",
    "ServerError",
    "Status",
    "SubscriptionAck",
    "TokenProvider",
    "TokenSource",
    "TerminalEffect",
    "UINT64_MAX",
    "UnknownEvent",
    "UnknownServerMessage",
    "Viewer",
    "ViewerTicket",
    "Window",
    "Windows",
    "XenoteerClient",
    "XenoteerError",
    "XenoteerEvent",
    "as_uint64_string",
    "admit_request_version",
    "classify_terminal_effect",
    "decode_event_message",
    "decode_server_message",
    "decode_uint64",
    "encode_uint64",
    "negotiate_protocol",
    "validate_status",
    "validate_client_command_envelope",
]

__version__ = "0.1.0"
