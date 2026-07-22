#!/usr/bin/python3
# SPDX-License-Identifier: BUSL-1.1
"""Unit tests for the configurable RFB WebSocket probe boundary."""

from __future__ import annotations

import importlib.util
import json
import pathlib
import tempfile
import unittest
from unittest import mock


PROBE_PATH = pathlib.Path(__file__).parents[1] / "rfb_websocket_probe.py"
SPEC = importlib.util.spec_from_file_location("rfb_websocket_probe", PROBE_PATH)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("could not load the RFB WebSocket probe module")
PROBE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PROBE)


class FakeHTTPResponse:
    def __init__(self, status: int, payload: bytes) -> None:
        self.status = status
        self._payload = payload

    def __enter__(self) -> "FakeHTTPResponse":
        return self

    def __exit__(self, *_arguments: object) -> None:
        return None

    def read(self, _limit: int) -> bytes:
        return self._payload


class AuthenticatedAPIJSONTests(unittest.TestCase):
    def request(self, expected_status: int) -> dict[str, object]:
        return PROBE._authenticated_api_json(
            "127.0.0.1",
            8080,
            "/v1/test",
            "http://127.0.0.1:8080",
            f"Bearer {'H' * 32}",
            expected_status=expected_status,
        )

    def test_accepts_explicit_200_status(self) -> None:
        with mock.patch.object(
            PROBE.urllib.request,
            "urlopen",
            return_value=FakeHTTPResponse(200, b'{"kind":"status"}'),
        ):
            self.assertEqual(self.request(200), {"kind": "status"})

    def test_accepts_explicit_201_status(self) -> None:
        with mock.patch.object(
            PROBE.urllib.request,
            "urlopen",
            return_value=FakeHTTPResponse(201, b'{"kind":"ticket"}'),
        ):
            self.assertEqual(self.request(201), {"kind": "ticket"})

    def test_rejects_a_valid_but_unexpected_status(self) -> None:
        with mock.patch.object(
            PROBE.urllib.request,
            "urlopen",
            return_value=FakeHTTPResponse(200, b'{"kind":"ticket"}'),
        ):
            with self.assertRaises(PROBE.ProbeError):
                self.request(201)


class WebSocketAggregateBoundsTests(unittest.TestCase):
    def websocket(
        self,
        frames: list[tuple[bool, int, bytes]],
        rfb_buffer: bytes = b"",
    ) -> object:
        websocket = PROBE.WebSocket.__new__(PROBE.WebSocket)
        websocket._rfb_buffer = bytearray(rfb_buffer)
        websocket._recv_frame = mock.Mock(side_effect=frames)
        return websocket

    def test_generic_fragmented_message_has_an_aggregate_bound(self) -> None:
        websocket = self.websocket(
            [(False, 0x2, b"abc"), (True, 0x0, b"de")]
        )
        with mock.patch.object(PROBE, "MAX_WEBSOCKET_PAYLOAD", 4):
            with self.assertRaises(PROBE.ProbeError):
                websocket.recv_message()

    def test_rfb_fragmented_message_has_an_aggregate_bound(self) -> None:
        websocket = self.websocket(
            [(False, 0x2, b"abc"), (True, 0x0, b"de")]
        )
        with mock.patch.object(PROBE, "MAX_WEBSOCKET_PAYLOAD", 4):
            with self.assertRaises(PROBE.ProbeError):
                websocket.recv_rfb(1)

    def test_rfb_buffer_has_an_aggregate_bound_across_messages(self) -> None:
        websocket = self.websocket([(True, 0x2, b"de")], rfb_buffer=b"abc")
        with mock.patch.object(PROBE, "MAX_WEBSOCKET_PAYLOAD", 4):
            with self.assertRaises(PROBE.ProbeError):
                websocket.recv_rfb(4)


class ViewerProtocolsTests(unittest.TestCase):
    def test_direct_mode_requests_only_binary(self) -> None:
        self.assertEqual(PROBE.viewer_protocols(None), ("binary",))

    def test_gateway_mode_reads_ticket_from_file(self) -> None:
        ticket = "A" * 43
        with tempfile.TemporaryDirectory() as directory:
            ticket_path = pathlib.Path(directory) / "ticket"
            ticket_path.write_text(f"{ticket}\n", encoding="ascii")

            self.assertEqual(
                PROBE.viewer_protocols(str(ticket_path)),
                ("binary", f"xenoteer.ticket.{ticket}"),
            )

    def test_gateway_mode_rejects_malformed_ticket_without_echoing_it(self) -> None:
        malformed = "forbidden.ticket.material"
        with tempfile.TemporaryDirectory() as directory:
            ticket_path = pathlib.Path(directory) / "ticket"
            ticket_path.write_text(malformed, encoding="ascii")

            with self.assertRaises(PROBE.ProbeError) as raised:
                PROBE.viewer_protocols(str(ticket_path))

        self.assertNotIn(malformed, str(raised.exception))

    def test_gateway_mode_rejects_oversized_ticket_file(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            ticket_path = pathlib.Path(directory) / "ticket"
            ticket_path.write_bytes(b"A" * 129)

            with self.assertRaises(PROBE.ProbeError):
                PROBE.viewer_protocols(str(ticket_path))


class MintViewerTicketTests(unittest.TestCase):
    def test_mint_uses_authenticated_status_and_ticket_routes(self) -> None:
        desktop_id = "11111111-1111-4111-8111-111111111111"
        desktop_generation = "22222222-2222-4222-8222-222222222222"
        ticket = "B" * 43
        responses = [
            {
                "desktop": {
                    "id": desktop_id,
                    "generation": desktop_generation,
                    "state": "ready",
                }
            },
            {
                "ticket": ticket,
                "desktop_id": desktop_id,
                "desktop_generation": desktop_generation,
                "origin": "http://127.0.0.1:8080",
                "audience": "viewer_websocket",
                "mode": "view_only",
                "use_policy": "single_use",
            },
        ]
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            api_token_path = root / "api-token"
            ticket_path = root / "ticket"
            metadata_path = root / "metadata"
            api_token_path.write_text("C" * 32, encoding="ascii")
            with mock.patch.object(
                PROBE,
                "_authenticated_api_json",
                side_effect=responses,
            ) as request:
                evidence = PROBE.mint_viewer_ticket(
                    "127.0.0.1",
                    8080,
                    "http://127.0.0.1:8080",
                    str(api_token_path),
                    str(ticket_path),
                    str(metadata_path),
                )

            self.assertEqual(request.call_count, 2)
            status_call, ticket_call = request.call_args_list
            self.assertEqual(status_call.args[2], "/v1/status")
            self.assertEqual(status_call.args[4], f"Bearer {'C' * 32}")
            self.assertEqual(status_call.kwargs, {"expected_status": 200})
            self.assertEqual(
                ticket_call.args[2],
                f"/v1/desktops/{desktop_id}/viewer-tickets",
            )
            self.assertEqual(
                ticket_call.args[5],
                {
                    "desktop_id": desktop_id,
                    "desktop_generation": desktop_generation,
                    "mode": "view_only",
                },
            )
            self.assertEqual(ticket_call.kwargs, {"expected_status": 201})
            self.assertEqual(ticket_path.read_text(encoding="ascii"), f"{ticket}\n")
            self.assertEqual(ticket_path.stat().st_mode & 0o777, 0o600)
            metadata = json.loads(metadata_path.read_text(encoding="ascii"))
            self.assertEqual(
                metadata["gateway_path"],
                f"/v1/desktops/{desktop_id}/generations/"
                f"{desktop_generation}/viewer/ws",
            )
            self.assertEqual(
                evidence,
                {"authenticated_api": True, "ticket_minted": True},
            )

    def test_mint_rejects_malformed_api_token_without_echoing_it(self) -> None:
        malformed = "token with forbidden whitespace"
        with tempfile.TemporaryDirectory() as directory:
            api_token_path = pathlib.Path(directory) / "api-token"
            api_token_path.write_text(malformed, encoding="ascii")
            with self.assertRaises(PROBE.ProbeError) as raised:
                PROBE.mint_viewer_ticket(
                    "127.0.0.1",
                    8080,
                    "http://127.0.0.1:8080",
                    str(api_token_path),
                    str(pathlib.Path(directory) / "ticket"),
                    str(pathlib.Path(directory) / "metadata"),
                )

        self.assertNotIn(malformed, str(raised.exception))

    def test_mint_never_overwrites_an_existing_private_output(self) -> None:
        desktop_id = "11111111-1111-4111-8111-111111111111"
        desktop_generation = "22222222-2222-4222-8222-222222222222"
        responses = [
            {
                "desktop": {
                    "id": desktop_id,
                    "generation": desktop_generation,
                    "state": "ready",
                }
            },
            {
                "ticket": "D" * 43,
                "desktop_id": desktop_id,
                "desktop_generation": desktop_generation,
                "origin": "http://127.0.0.1:8080",
                "audience": "viewer_websocket",
                "mode": "view_only",
                "use_policy": "single_use",
            },
        ]
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            api_token_path = root / "api-token"
            ticket_path = root / "ticket"
            metadata_path = root / "metadata"
            api_token_path.write_text("E" * 32, encoding="ascii")
            ticket_path.write_text("existing-private-file\n", encoding="ascii")
            with mock.patch.object(
                PROBE,
                "_authenticated_api_json",
                side_effect=responses,
            ):
                with self.assertRaises(PROBE.ProbeError):
                    PROBE.mint_viewer_ticket(
                        "127.0.0.1",
                        8080,
                        "http://127.0.0.1:8080",
                        str(api_token_path),
                        str(ticket_path),
                        str(metadata_path),
                    )

            self.assertEqual(
                ticket_path.read_text(encoding="ascii"),
                "existing-private-file\n",
            )
            self.assertFalse(metadata_path.exists())

    def test_mint_removes_new_ticket_if_metadata_output_already_exists(self) -> None:
        desktop_id = "11111111-1111-4111-8111-111111111111"
        desktop_generation = "22222222-2222-4222-8222-222222222222"
        responses = [
            {
                "desktop": {
                    "id": desktop_id,
                    "generation": desktop_generation,
                    "state": "ready",
                }
            },
            {
                "ticket": "F" * 43,
                "desktop_id": desktop_id,
                "desktop_generation": desktop_generation,
                "origin": "http://127.0.0.1:8080",
                "audience": "viewer_websocket",
                "mode": "view_only",
                "use_policy": "single_use",
            },
        ]
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            api_token_path = root / "api-token"
            ticket_path = root / "ticket"
            metadata_path = root / "metadata"
            api_token_path.write_text("G" * 32, encoding="ascii")
            metadata_path.write_text("existing-metadata\n", encoding="ascii")
            with mock.patch.object(
                PROBE,
                "_authenticated_api_json",
                side_effect=responses,
            ):
                with self.assertRaises(PROBE.ProbeError):
                    PROBE.mint_viewer_ticket(
                        "127.0.0.1",
                        8080,
                        "http://127.0.0.1:8080",
                        str(api_token_path),
                        str(ticket_path),
                        str(metadata_path),
                    )

            self.assertFalse(ticket_path.exists())
            self.assertEqual(
                metadata_path.read_text(encoding="ascii"),
                "existing-metadata\n",
            )


if __name__ == "__main__":
    unittest.main()
