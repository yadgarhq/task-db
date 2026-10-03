"""The chart's whole key set is closed: `chart/values.schema.json` (ledger 990,
ADR-0847, ADR-0850).

WHAT THIS FILE ADDS, AND WHAT IT DOES NOT. Before this change the schema bounded
exactly one knob (`database.migrationLockTimeoutSeconds`, ledger 814,
`test_migration_lock_schema.py`) and nothing else was validated at render. This file
adds the OTHER property: every key this chart's `values.yaml` declares is now
enumerated, so a typo anywhere is refused at render naming the key and its JSON
path, instead of being silently accepted and read as `nil`/zero by the binary or a
template. `test_migration_lock_schema.py` and `test_render_checks.py` /
`test_mariadb.py` already cover the bound and the boolean-shaped toggles
respectively; this file does not re-assert either.

TESTS HERE ASSERT THE SCHEMA KEY AND ITS JSON PATH ONLY, NEVER HELM'S OWN WORDING.
helm's JSON-Schema error sentence is the validator library's text, not this
chart's, and it has changed shape across helm releases — MEASURED, not assumed,
against TWO live installs: 3.20.2 and 4.3.0 both print `at '<path>': additional
properties 'X' not allowed` (slash-separated path, root `''`), while this
repository's own `ci / precommit` job (`azure/setup-helm@…` pins `v3.18.4`)
prints `- <path>: Additional property X is not allowed` (dot-separated path,
root the literal string `(root)`). `extract_refusal` below parses EITHER shape
into the same (path-segment-tuple, key) pair, and every assertion compares that
pair — never the sentence around it — so neither a local run against 3.20.2/4.3.0
nor the CI run against 3.18.4 reddens this file for a reason that has nothing to
do with the schema.

CLOSED, OPEN, AND EXTRA are the vocabulary `values.schema.json`'s own `$comment`
uses and this file reuses verbatim: CLOSED is a block whose `additionalProperties`
is `false`; OPEN is a block left as a bare `{}` so any subkey passes; EXTRA is a
schema leaf this chart's `values.yaml` does not declare, added because a template
reads it anyway (`image.digest`, `networkPolicy.scrapeFrom.namespace`).

Run: python3 -m pytest scripts/tests/ -q
"""

from __future__ import annotations

import copy
import json
import re
import shutil
import subprocess
from pathlib import Path
from typing import Any

import yaml

REPO = Path(__file__).resolve().parents[2]
CHART = REPO / "chart"
SCHEMA_PATH = CHART / "values.schema.json"
VALUES_PATH = CHART / "values.yaml"

# THE EXACT SET OF PATHS THIS SCHEMA LEAVES OPEN (a bare `{}` where `values.yaml`
# itself holds a MAPPING) — the per-chart table's "Open" column for the -db twins.
# `global` is forced open though `values.yaml` never declares it at all (the parent
# forwards it to every child); the rest are read with `toYaml`/`with` and never
# enumerated by any template in this chart.
OPEN_PATHS = {
    ("global",),
    ("resources",),
    ("rollingUpdate",),
    ("database", "instance", "resources"),
}

# THE EXACT SET OF SCHEMA LEAVES `values.yaml` DOES NOT DECLARE — the per-chart
# table's "Extras" column. Each is read by exactly one template, named in
# `values.schema.json`'s own `$comment`.
EXTRA_PATHS = {
    ("image", "digest"),
    ("networkPolicy", "scrapeFrom", "namespace"),
}

# THE TWO MESSAGE SHAPES MEASURED: 3.20.2 and 4.3.0 (this builder's own pair,
# `at '/path': additional properties 'key' not allowed`, PATH SLASH-SEPARATED,
# root `''`) and 3.18.4 (what this repository's own `ci / precommit` job pins via
# `azure/setup-helm`, `- path: Additional property key is not allowed`, PATH
# DOT-SEPARATED, root the literal string `(root)`). Both are parsed into the SAME
# shape — a tuple of path segments plus the bare key name — so every assertion
# below compares that shape and never either sentence.
REFUSAL_SLASH_PATH = re.compile(
    r"at '([^']*)': additional propert(?:y|ies) '([^']+)' (?:is |are )?not allowed"
)
REFUSAL_DOTTED_PATH = re.compile(
    r"^-\s+(\(root\)|[A-Za-z0-9_.\-]+):\s+Additional propert(?:y|ies)\s+(\S+)\s+(?:is|are)\s+not allowed",
    re.MULTILINE,
)


def load_schema() -> dict[str, Any]:
    return json.loads(SCHEMA_PATH.read_text())


def load_values() -> dict[str, Any]:
    return yaml.safe_load(VALUES_PATH.read_text()) or {}


def helm(*arguments: str) -> subprocess.CompletedProcess[str]:
    binary = shutil.which("helm")
    assert binary, (
        "helm is not on PATH. This suite renders the chart, and so does the "
        "`helm lint and render` pre-commit hook — install helm rather than skip."
    )
    return subprocess.run([binary, *arguments], capture_output=True, text=True)


def render(*arguments: str) -> subprocess.CompletedProcess[str]:
    return helm("template", "task-db", str(CHART), *arguments)


def overlay(body: str, destination: Path) -> Path:
    destination.mkdir(parents=True, exist_ok=True)
    path = destination / "values.yaml"
    path.write_text(body)
    assert path.read_text() == body
    return path


def render_overlay(body: str, destination: Path, *extra: str) -> subprocess.CompletedProcess[str]:
    values = overlay(body, destination)
    return render("--values", str(values), *extra)


def extract_refusal(stderr: str) -> tuple[tuple[str, ...], str] | None:
    """The JSON path (as a tuple of segments) and the key name out of a schema
    refusal — on EITHER measured helm shape — never the sentence around them.
    """
    match = REFUSAL_SLASH_PATH.search(stderr)
    if match:
        raw_path, key = match.group(1), match.group(2)
        segments = tuple(raw_path.strip("/").split("/")) if raw_path.strip("/") else ()
        return segments, key

    match = REFUSAL_DOTTED_PATH.search(stderr)
    if match:
        raw_path, key = match.group(1), match.group(2)
        segments = () if raw_path == "(root)" else tuple(raw_path.split("."))
        return segments, key

    return None


def object_count(stdout: str) -> int:
    return len(
        [doc for doc in yaml.safe_load_all(stdout) if isinstance(doc, dict) and doc.get("apiVersion")]
    )


# ── RENDER: THE CLOSURE REFUSES A TYPO, NAMING THE KEY AND THE PATH ──────────────


def test_a_root_typo_is_refused_naming_the_key_at_the_root_path(tmp_path: Path) -> None:
    result = render_overlay("autoscalng:\n  enabled: true\n", tmp_path)
    assert result.returncode != 0, result.stdout
    found = extract_refusal(result.stderr)
    assert found, result.stderr
    path, key = found
    assert path == (), (path, result.stderr)
    assert key == "autoscalng", (key, result.stderr)


def test_a_typo_one_level_down_is_refused_naming_the_key_under_its_block(tmp_path: Path) -> None:
    result = render_overlay("autoscaling:\n  enabeld: true\n", tmp_path)
    assert result.returncode != 0, result.stdout
    found = extract_refusal(result.stderr)
    assert found, result.stderr
    path, key = found
    assert path == ("autoscaling",), (path, result.stderr)
    assert key == "enabeld", (key, result.stderr)


def test_a_typo_two_levels_down_in_scrapefrom_is_refused(tmp_path: Path) -> None:
    result = render_overlay(
        "networkPolicy:\n  scrapeFrom:\n    namespac: observability\n", tmp_path
    )
    assert result.returncode != 0, result.stdout
    found = extract_refusal(result.stderr)
    assert found, result.stderr
    path, key = found
    assert path == ("networkPolicy", "scrapeFrom"), (path, result.stderr)
    assert key == "namespac", (key, result.stderr)


def test_a_typo_two_levels_down_in_the_instance_storage_block_is_refused(tmp_path: Path) -> None:
    """The twins' own extra row (brief §5): `database.instance.storage` is closed."""
    result = render_overlay(
        "database:\n  instance:\n    storage:\n      siz: 2Gi\n", tmp_path
    )
    assert result.returncode != 0, result.stdout
    found = extract_refusal(result.stderr)
    assert found, result.stderr
    path, key = found
    assert path == ("database", "instance", "storage"), (path, result.stderr)
    assert key == "siz", (key, result.stderr)


# ── RENDER: WHAT STAYS OPEN OR EXTRA RENDERS CLEANLY ─────────────────────────────


def test_every_open_map_accepts_an_arbitrary_subkey_with_object_count_unchanged(
    tmp_path: Path,
) -> None:
    baseline = render()
    assert baseline.returncode == 0, baseline.stderr
    expected = object_count(baseline.stdout)

    bodies = {
        "global": "global:\n  whatever: 1\n",
        "resources": "resources:\n  foo:\n    bar: 1\n",
        "rollingUpdate": "rollingUpdate:\n  partition: 1\n",
        "database.instance.resources": "database:\n  instance:\n    resources:\n      foo: 1\n",
    }
    assert set(".".join(p) for p in OPEN_PATHS) == set(bodies), "a row is missing for an OPEN path"

    for label, body in bodies.items():
        result = render_overlay(body, tmp_path / label)
        assert result.returncode == 0, f"{label}: {result.stderr}"
        assert object_count(result.stdout) == expected, (
            f"{label}: rendered {object_count(result.stdout)} objects, expected {expected}"
        )


def test_the_declared_extras_are_accepted(tmp_path: Path) -> None:
    result = render_overlay("image:\n  digest: sha256:" + "a" * 64 + "\n", tmp_path / "image")
    assert result.returncode == 0, result.stderr

    result = render_overlay(
        "networkPolicy:\n  scrapeFrom:\n    namespace: observability\n",
        tmp_path / "scrapefrom",
    )
    assert result.returncode == 0, result.stderr


def test_an_untyped_leaf_accepts_a_writable_value(tmp_path: Path) -> None:
    result = render("--set-string", "replicaCount=2")
    assert result.returncode == 0, result.stderr


def test_lint_refuses_the_root_typo_naming_the_key(tmp_path: Path) -> None:
    values = overlay("autoscalng:\n  enabled: true\n", tmp_path)
    binary = shutil.which("helm")
    assert binary
    result = subprocess.run(
        [binary, "lint", "--strict", str(CHART), "-f", str(values)],
        capture_output=True,
        text=True,
    )
    assert result.returncode != 0, result.stdout
    assert "autoscalng" in (result.stdout + result.stderr)


# ── STRUCTURAL (PURE, NO HELM): THE SCHEMA FILE ITSELF ───────────────────────────


def closed_blocks_missing_additional_properties_false(schema: dict[str, Any]) -> list[str]:
    """Every node carrying `properties` must carry `additionalProperties: false`."""
    violations: list[str] = []

    def walk(node: Any, path: str) -> None:
        if not isinstance(node, dict):
            return
        if "properties" in node:
            if node.get("additionalProperties") is not False:
                violations.append(path or "<root>")
            for key, child in node["properties"].items():
                walk(child, f"{path}.{key}" if path else key)

    walk(schema, "")
    return violations


def actual_open_map_paths(schema: dict[str, Any], values: dict[str, Any]) -> set[tuple[str, ...]]:
    """Every path where `values.yaml` holds a mapping but the schema leaves it `{}`.

    `global` is checked separately: `values.yaml` never declares it, so it cannot be
    found by walking `values.yaml`.
    """
    found: set[tuple[str, ...]] = set()

    def walk(value: Any, node: Any, path: tuple[str, ...]) -> None:
        if not isinstance(value, dict):
            return
        if node == {}:
            found.add(path)
            return
        props = node.get("properties", {}) if isinstance(node, dict) else {}
        for key, child_value in value.items():
            walk(child_value, props.get(key, {}), path + (key,))

    walk(values, schema, ())
    if schema.get("properties", {}).get("global") == {}:
        found.add(("global",))
    return found


def undeclared_values_yaml_leaves(
    schema: dict[str, Any], values: dict[str, Any]
) -> list[tuple[str, ...]]:
    """Every `values.yaml` leaf path not reachable through the schema."""
    missing: list[tuple[str, ...]] = []

    def walk(value: Any, node: Any, path: tuple[str, ...]) -> None:
        if node == {}:
            return  # an open map or an untyped leaf — everything below it is covered
        if isinstance(value, dict) and value:
            props = node.get("properties", {}) if isinstance(node, dict) else {}
            for key, child_value in value.items():
                if key not in props:
                    missing.append(path + (key,))
                else:
                    walk(child_value, props[key], path + (key,))
            return
        # a scalar, a list, or an empty mapping: it must be declared as a leaf,
        # which `node == {}` above already covers when it is.

    walk(values, schema, ())
    return missing


def schema_leaves(schema: dict[str, Any]) -> dict[tuple[str, ...], Any]:
    """Every leaf (`{}` or a typed node with no `properties`) the schema declares,
    excluding anything inside an OPEN path (an open map's own subkeys are never
    enumerated, so they are not "schema leaves")."""
    leaves: dict[tuple[str, ...], Any] = {}

    def walk(node: Any, path: tuple[str, ...]) -> None:
        if not isinstance(node, dict):
            return
        if path in OPEN_PATHS:
            return
        if "properties" in node:
            for key, child in node["properties"].items():
                walk(child, path + (key,))
        elif path:
            leaves[path] = node

    walk(schema, ())
    return leaves


def value_at(values: dict[str, Any], path: tuple[str, ...]) -> tuple[bool, Any]:
    cursor: Any = values
    for step in path:
        if not isinstance(cursor, dict) or step not in cursor:
            return False, None
        cursor = cursor[step]
    return True, cursor


def declared(schema: dict[str, Any], path: tuple[str, ...]) -> bool:
    cursor: Any = schema
    for step in path:
        if not isinstance(cursor, dict) or "properties" not in cursor or step not in cursor["properties"]:
            return False
        cursor = cursor["properties"][step]
    return True


def test_every_closed_block_carries_additional_properties_false() -> None:
    violations = closed_blocks_missing_additional_properties_false(load_schema())
    assert violations == [], violations


def test_exactly_the_documented_paths_are_open_maps() -> None:
    schema = load_schema()
    values = load_values()
    assert actual_open_map_paths(schema, values) == OPEN_PATHS


def test_every_values_yaml_leaf_is_declared_in_the_schema() -> None:
    missing = undeclared_values_yaml_leaves(load_schema(), load_values())
    assert missing == [], missing


def test_every_schema_leaf_absent_from_values_yaml_is_a_documented_extra() -> None:
    schema = load_schema()
    values = load_values()
    undeclared_in_values = []
    for path in schema_leaves(schema):
        present, _ = value_at(values, path)
        if not present:
            undeclared_in_values.append(path)
    assert set(undeclared_in_values) == EXTRA_PATHS, undeclared_in_values


def test_global_is_declared_open() -> None:
    schema = load_schema()
    assert "global" in schema.get("properties", {}), "global is not declared"
    assert schema["properties"]["global"] == {}, "global is declared but not left open"


def test_every_documented_extra_is_actually_declared() -> None:
    schema = load_schema()
    for path in EXTRA_PATHS:
        assert declared(schema, path), f"{'.'.join(path)} is listed as an extra but not declared"


# ── MUTATIONS: EACH STRUCTURAL CHECK ABOVE MUST REDDEN WHEN THE PROPERTY IT TESTS
#    IS ACTUALLY BROKEN (ledger 990 Yadgar contract: mutation-check the key
#    assertion) ──────────────────────────────────────────────────────────────────


def test_deleting_the_root_additional_properties_reddens_the_closure_check() -> None:
    schema = load_schema()
    del schema["additionalProperties"]
    violations = closed_blocks_missing_additional_properties_false(schema)
    assert violations == ["<root>"], violations


def test_deleting_global_reddens_the_global_check() -> None:
    schema = load_schema()
    del schema["properties"]["global"]
    assert "global" not in schema["properties"]


def test_deleting_an_extra_reddens_the_extras_check() -> None:
    schema = load_schema()
    del schema["properties"]["image"]["properties"]["digest"]
    assert not declared(schema, ("image", "digest"))


def test_closing_an_open_map_reddens_the_open_paths_check() -> None:
    schema = load_schema()
    values = load_values()
    schema["properties"]["resources"] = {"properties": {}, "additionalProperties": False}
    assert actual_open_map_paths(schema, values) != OPEN_PATHS
    # and it reddens the declared-leaves check too: `values.yaml`'s own
    # `resources.requests.cpu` etc. are no longer reachable through the closed,
    # empty block.
    missing = undeclared_values_yaml_leaves(schema, values)
    assert missing, "closing `resources` to an empty block was expected to orphan its leaves"


def test_the_shipped_schema_is_untouched_by_the_mutation_helpers() -> None:
    """A tripwire: every mutation test above operates on a FRESH `load_schema()`
    copy. If any of them mutated the module-level file on disk instead, every test
    after it in file order would see the damage. `copy.deepcopy` is not even
    needed because `load_schema()` always re-reads from disk — this test exists so
    that stops being true silently.
    """
    before = load_schema()
    mutated = copy.deepcopy(before)
    del mutated["properties"]["global"]
    after = load_schema()
    assert after == before
    assert "global" in after["properties"]
