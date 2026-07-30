# SPDX-License-Identifier: Apache-2.0
"""Fail-closed contracts for executable selector shapes in the package README."""

from __future__ import annotations

import ast
import copy
import re
import textwrap
import tomllib
import unittest
from collections.abc import Mapping
from pathlib import Path
from typing import Any

from xenoteer.examples.phase6_behaviors import (
    GTK_TITLE,
    element_selector,
    window_selector,
)


PACKAGE_ROOT = Path(__file__).resolve().parents[1]
README_PATH = PACKAGE_ROOT / "README.md"
PYPROJECT_PATH = PACKAGE_ROOT / "pyproject.toml"
SELECTOR_SECTION = "## Windows, accessibility, capture, and viewing"


def _selector_example() -> str:
    """Extract the executable Python block from the published selector section."""

    readme = README_PATH.read_text(encoding="utf-8")
    try:
        section = readme.split(SELECTOR_SECTION, 1)[1]
    except IndexError as error:
        raise AssertionError("package README omitted the selector section") from error
    section = section.split("\n## ", 1)[0]
    blocks = re.findall(r"(?ms)^```python\n(?P<source>.*?)^```$", section)
    if len(blocks) != 1:
        raise AssertionError(
            f"selector section must contain exactly one Python block, found {len(blocks)}"
        )
    return blocks[0]


def _attribute_path(expression: ast.expr) -> tuple[str, ...]:
    parts: list[str] = []
    while isinstance(expression, ast.Attribute):
        parts.append(expression.attr)
        expression = expression.value
    if isinstance(expression, ast.Name):
        parts.append(expression.id)
    return tuple(reversed(parts))


def _readme_selectors() -> dict[str, object]:
    """Read literal selectors passed to the exact public README calls."""

    tree = ast.parse(_selector_example(), filename=str(README_PATH))
    selectors: dict[str, object] = {}
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call) or not node.args:
            continue
        path = _attribute_path(node.func)
        if path == ("desktop", "windows", "one"):
            selectors["window"] = ast.literal_eval(node.args[0])
        elif path == ("desktop", "accessibility", "one"):
            selectors["accessibility"] = ast.literal_eval(node.args[0])
    if set(selectors) != {"window", "accessibility"}:
        raise AssertionError(
            "selector example must call windows.one() and accessibility.one() once"
        )
    return selectors


def compile_selector_example() -> None:
    """Compile the actual top-level-await snippet in its documented async context."""

    wrapped = "async def _readme_selector_example(desktop):\n" + textwrap.indent(
        _selector_example(),
        "    ",
    )
    compile(wrapped, str(README_PATH), "exec")


def _require_keys(value: object, expected: set[str], label: str) -> Mapping[str, Any]:
    if not isinstance(value, Mapping):
        raise AssertionError(f"{label} must be an object")
    observed = set(value)
    if observed != expected:
        raise AssertionError(
            f"{label} fields must be exactly {sorted(expected)!r}, "
            f"observed {sorted(observed)!r}"
        )
    if not all(isinstance(key, str) for key in value):
        raise AssertionError(f"{label} field names must be strings")
    return value


def validate_window_selector(value: object) -> None:
    """Validate the exact frozen-v1 window selector shape used by the README."""

    selector = _require_keys(value, {"type", "predicate"}, "window selector")
    if selector["type"] != "predicate":
        raise AssertionError("window selector type must be predicate")
    predicate = _require_keys(
        selector["predicate"],
        {"type", "field", "matcher"},
        "window predicate",
    )
    if predicate["type"] != "text" or predicate["field"] != "title":
        raise AssertionError("window predicate must match title text")
    matcher = _require_keys(
        predicate["matcher"],
        {"type", "value", "case_sensitive"},
        "window matcher",
    )
    if (
        matcher["type"] != "exact"
        or matcher["value"] != GTK_TITLE
        or matcher["case_sensitive"] is not True
    ):
        raise AssertionError("window matcher must target the qualified GTK fixture")


def validate_accessibility_selector(value: object) -> None:
    """Validate the exact frozen-v1 accessibility selector shape."""

    selector = _require_keys(
        value,
        {"scope", "predicates", "order", "result_index"},
        "accessibility selector",
    )
    scope = _require_keys(selector["scope"], {"type"}, "accessibility scope")
    if scope["type"] != "desktop":
        raise AssertionError("accessibility scope must be desktop")
    predicates = selector["predicates"]
    if not isinstance(predicates, list) or len(predicates) != 1:
        raise AssertionError("accessibility selector must have one predicate")
    predicate = _require_keys(
        predicates[0],
        {"type", "matcher"},
        "accessibility name predicate",
    )
    if predicate["type"] != "name":
        raise AssertionError("accessibility predicate must select by name")
    matcher = _require_keys(
        predicate["matcher"],
        {"type", "value", "case_sensitive"},
        "accessibility name matcher",
    )
    if (
        matcher["type"] != "exact"
        or matcher["value"] != "Stable Button"
        or matcher["case_sensitive"] is not True
    ):
        raise AssertionError("accessibility matcher must target the qualified GTK fixture")
    if (
        selector["order"] != "object_path_ascending"
        or selector["result_index"] is not None
    ):
        raise AssertionError(
            "accessibility selector must use stable order without result indexing"
        )


class PackageReadmeTests(unittest.TestCase):
    """Keep package documentation executable against the frozen protocol."""

    def test_pyproject_publishes_the_validated_readme(self) -> None:
        pyproject = tomllib.loads(PYPROJECT_PATH.read_text(encoding="utf-8"))
        self.assertEqual(pyproject["project"]["readme"], README_PATH.name)

    def test_metadata_claims_every_ci_qualified_python_minor(self) -> None:
        pyproject = tomllib.loads(PYPROJECT_PATH.read_text(encoding="utf-8"))
        self.assertEqual(pyproject["project"]["requires-python"], ">=3.11")
        classifiers = set(pyproject["project"]["classifiers"])
        self.assertTrue(
            {
                f"Programming Language :: Python :: 3.{minor}"
                for minor in range(11, 15)
            }.issubset(classifiers)
        )

    def test_window_selector_uses_the_frozen_v1_shape(self) -> None:
        compile_selector_example()
        validate_window_selector(_readme_selectors()["window"])

    def test_accessibility_selector_uses_the_frozen_v1_shape(self) -> None:
        compile_selector_example()
        validate_accessibility_selector(_readme_selectors()["accessibility"])

    def test_readme_selectors_are_the_live_qualified_example_selectors(self) -> None:
        selectors = _readme_selectors()
        self.assertEqual(selectors["window"], window_selector(GTK_TITLE))
        self.assertEqual(
            selectors["accessibility"],
            element_selector("Stable Button"),
        )

    def test_selector_contract_rejects_obsolete_aliases_and_unknown_fields(
        self,
    ) -> None:
        selectors = _readme_selectors()
        window = copy.deepcopy(selectors["window"])
        assert isinstance(window, dict)
        matcher = window["predicate"]["matcher"]
        matcher["mode"] = matcher.pop("type", "contains")
        with self.assertRaisesRegex(AssertionError, "window matcher fields"):
            validate_window_selector(window)

        obsolete_accessibility = copy.deepcopy(selectors["accessibility"])
        assert isinstance(obsolete_accessibility, dict)
        obsolete_accessibility["root"] = obsolete_accessibility.pop("scope")
        predicates = obsolete_accessibility.pop("predicates")
        obsolete_accessibility["predicate"] = predicates[0]
        with self.assertRaisesRegex(AssertionError, "accessibility selector fields"):
            validate_accessibility_selector(obsolete_accessibility)

        accessibility = copy.deepcopy(selectors["accessibility"])
        assert isinstance(accessibility, dict)
        accessibility["future"] = True
        with self.assertRaisesRegex(AssertionError, "accessibility selector fields"):
            validate_accessibility_selector(accessibility)

    def test_selector_prose_distinguishes_strict_inputs_from_additive_responses(
        self,
    ) -> None:
        readme = README_PATH.read_text(encoding="utf-8")
        self.assertIn("Client-authored selector dictionaries are strict", readme)
        self.assertRegex(
            readme,
            r"Server-authored response dictionaries\s+preserve additive fields",
        )


if __name__ == "__main__":
    unittest.main()
