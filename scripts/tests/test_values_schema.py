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
CI_VALUES_PATH = CHART / "ci" / "values.yaml"

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
    # B-U5E (ADR-0854), folded into C-DB1: declared so an adopter's override is
    # validated by the closure, but `values.yaml` ships neither. `clientAuth`
    # moved to REQUIRED_NO_DEFAULT below in B-U5.
    ("tls", "clientCaSecret"),
    ("tls", "clientCaSecretKey"),
}

# THE EXACT SET OF SCHEMA LEAVES THIS CHART REQUIRES AND SHIPS NO DEFAULT FOR
# (ADR-0845, C-DB1). `tls.enabled` is neither OPEN nor an EXTRA: it is a typed,
# `required` leaf that `values.yaml` deliberately does not declare, so the
# extras check below must not demand it appear in `EXTRA_PATHS` — doing that
# would blur "deliberately undeclared, no constraint" with "deliberately
# undeclared, and an adopter MUST choose". `test_values_yaml_ships_no_tls_
# enabled_default` and `test_every_required_no_default_key_is_required_in_
# the_schema` are this set's own two-sided proof: absent from `values.yaml`,
# present and `required` in the schema.
REQUIRED_NO_DEFAULT = {
    ("tls", "enabled"),
    # B-U5 (ADR-0854, X-ADR-1): the binary refuses to boot without
    # LISTEN_TLS_CLIENT_AUTH, so the chart has no default for the value that
    # renders it either.
    ("tls", "clientAuth"),
}

# THE TWO MESSAGE SHAPES MEASURED: 3.20.2 and 4.3.0 (this builder's own pair,
# `at '/path': additional properties 'key' not allowed`, PATH SLASH-SEPARATED,
# root `''`) and 3.18.4 (what this repository's own `ci / precommit` job pins via
# `azure/setup-helm`, `- path: Additional property key is not allowed`, PATH
# DOT-SEPARATED, root the literal string `(root)`). Both are parsed into the SAME
# shape — a tuple of path segments plus the bare key name — so every assertion
# below compares that shape and never either sentence.
REFUSAL_SLASH_PATH = re.compile(
    r"at '([^']*)': additional propert(?:y|ies) '([^']+)'(?:, '[^']+')* (?:is |are )?not allowed"
)
REFUSAL_DOTTED_PATH = re.compile(
    r"^-\s+(\(root\)|[A-Za-z0-9_.\-]+):\s+Additional propert(?:y|ies)\s+(\S+)\s+(?:is|are)\s+not allowed",
    re.MULTILINE,
)

# THE TWO SHAPES OF A SCHEMA `required`/`type` REFUSAL (ADR-0845, C-DB1),
# MEASURED THE SAME WAY as the pair above, on the same three helm versions,
# against `tls.enabled` specifically (`type: boolean`, `required: [enabled]`
# — the one leaf this card adds either keyword to):
#
#   type mismatch   (4.3.0, 3.20.2) "- at '/tls/enabled': got string, want boolean"
#                    (3.18.4)       "- tls.enabled: Invalid type. Expected: boolean, given: string"
#   missing property (4.3.0, 3.20.2) "- at '/tls': missing property 'enabled'"
#                    (3.18.4)       "- tls: enabled is required"
#
# Both are parsed into the SAME (path-segment-tuple, key) shape `extract_refusal`
# already uses, so `test_tls_enabled_*` below compares that shape and never the
# sentence (correction 3: a schema refusal is asserted on key and path only).
REFUSAL_SLASH_TYPE = re.compile(r"at '([^']*)': got \S+, want \S+")
REFUSAL_DOTTED_TYPE = re.compile(
    r"^-\s+(\(root\)|[A-Za-z0-9_.\-]+):\s+Invalid type\.", re.MULTILINE
)
# `missing properties 'enabled', 'clientAuth'` is how 3.20.2 and 4.3.0 report
# BOTH of `tls`'s required keys absent at once (B-U5, measured on a bare lint);
# the first named key is the one returned. 3.18.4 prints one line per key.
REFUSAL_SLASH_MISSING = re.compile(r"at '([^']*)': missing propert(?:y|ies) '([^']+)'")
REFUSAL_DOTTED_MISSING = re.compile(
    r"^-\s+(\(root\)|[A-Za-z0-9_.\-]+):\s+(\S+) is required", re.MULTILINE
)
# AN `enum` REFUSAL, measured the same way (4.3.0, 3.20.2, 3.18.4) while B-U5
# briefly carried one on `tls.clientAuth`; kept so a future enum leaf parses:
#   (4.3.0, 3.20.2) "- at '/tls/clientAuth': value must be one of 'off', 'optional', 'required'"
#   (3.18.4)        "- tls.clientAuth: tls.clientAuth must be one of the following: ..."
REFUSAL_SLASH_ENUM = re.compile(r"at '([^']*)': value must be one of")
REFUSAL_DOTTED_ENUM = re.compile(
    r"^-\s+([A-Za-z0-9_.\-]+):\s+\S+ must be one of the following", re.MULTILINE
)


def extract_type_or_missing_refusal(stderr: str) -> tuple[tuple[str, ...], str] | None:
    """The JSON path and the leaf key out of a schema `type` or `required`
    refusal, on EITHER measured helm shape. Mirrors `extract_refusal`, for the
    two shapes that one does not parse (see the regexes' own comment).
    """
    match = REFUSAL_SLASH_TYPE.search(stderr) or REFUSAL_SLASH_ENUM.search(stderr)
    if match:
        raw_path = match.group(1).strip("/")
        segments = tuple(raw_path.split("/")) if raw_path else ()
        if not segments:
            return None
        return segments[:-1], segments[-1]

    match = REFUSAL_DOTTED_TYPE.search(stderr) or REFUSAL_DOTTED_ENUM.search(stderr)
    if match:
        raw_path = match.group(1)
        if raw_path == "(root)":
            return None
        segments = tuple(raw_path.split("."))
        return segments[:-1], segments[-1]

    match = REFUSAL_SLASH_MISSING.search(stderr)
    if match:
        raw_path, key = match.group(1), match.group(2)
        segments = tuple(raw_path.strip("/").split("/")) if raw_path.strip("/") else ()
        return segments, key

    match = REFUSAL_DOTTED_MISSING.search(stderr)
    if match:
        raw_path, key = match.group(1), match.group(2)
        segments = () if raw_path == "(root)" else tuple(raw_path.split("."))
        return segments, key

    return None


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
    # `-f CI_VALUES_PATH` FIRST, ALWAYS, WHEN THE FILE EXISTS (ADR-0845,
    # C-DB1): `tls.enabled` carries no default any more, and this chart's own
    # `ci/values.yaml` is the one place that states the baseline every other
    # render in this file renders against — the same contract
    # `chart_values_override.py` documents for the shared `helm-lint` hook.
    # First, not last, so a case-specific `--values`/`--set` in `*arguments`
    # still wins on any key the two happen to share.
    override = ("-f", str(CI_VALUES_PATH)) if CI_VALUES_PATH.is_file() else ()
    return helm("template", "task-db", str(CHART), *override, *arguments)


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


# ── PURE (NO HELM): extract_refusal PARSES EVERY MEASURED REFUSAL SHAPE ──────────


def test_extract_refusal_parses_the_slash_path_multi_key_form() -> None:
    """A newer helm can refuse several unknown keys under one block in a single
    sentence, e.g. `at '/autoscaling': additional properties 'a', 'b' not
    allowed`. `REFUSAL_SLASH_PATH` must still match it, naming the path and the
    first offending key, instead of failing to match at all.
    """
    found = extract_refusal(
        "Error: INSTALLATION FAILED: ... at '/autoscaling': additional "
        "properties 'a', 'b' not allowed"
    )
    assert found, "the multi-key slash-path form was not recognised"
    path, key = found
    assert path == ("autoscaling",), (path, key)
    assert key == "a", (path, key)


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


# ── RENDER: `tls.enabled` CARRIES NO DEFAULT (ADR-0845, C-DB1) ───────────────────


def test_tls_enabled_wrong_type_is_refused_by_the_schema_naming_tls_enabled(
    tmp_path: Path,
) -> None:
    result = render_overlay('tls:\n  enabled: "true"\n', tmp_path)
    assert result.returncode != 0, result.stdout
    # THE STABLE WRAPPER (B-U5E-convention.md item 9), asserted alongside the
    # path and key rather than instead of them: every schema violation carries
    # it, on every measured helm version, so its presence is what tells a
    # schema refusal apart from a render-check one even before the shape is
    # parsed.
    assert "values don't meet the specifications of the schema" in result.stderr, result.stderr
    found = extract_type_or_missing_refusal(result.stderr)
    assert found, result.stderr
    path, key = found
    assert path == ("tls",), (path, result.stderr)
    assert key == "enabled", (key, result.stderr)


def test_tls_enabled_null_is_refused_by_the_schema_naming_tls_enabled(
    tmp_path: Path,
) -> None:
    """`enabled: null` is NOT the same shape as `tls: null` below: `values.yaml`
    carries no default for `enabled` any more, so helm's null-key-deletion
    (which only drops a key the chart's OWN defaults also set) does not apply
    to it — the null survives into the merged values as a value, and the
    schema refuses it as the wrong type rather than as a missing key.
    """
    result = render_overlay("tls:\n  enabled:\n", tmp_path)
    assert result.returncode != 0, result.stdout
    assert "values don't meet the specifications of the schema" in result.stderr, result.stderr
    found = extract_type_or_missing_refusal(result.stderr)
    assert found, result.stderr
    path, key = found
    assert path == ("tls",), (path, result.stderr)
    assert key == "enabled", (key, result.stderr)


def test_tls_block_null_is_refused_naming_tls_by_the_render_check(tmp_path: Path) -> None:
    """THE OTHER NULL SHAPE: `tls:` with no value deletes the WHOLE block this
    chart's `values.yaml` declares (it, unlike `enabled`, still carries
    defaults — `certSecret` etc.) — so this reaches `templates/render-
    checks.yaml`'s `tls`-absent arm, not the schema. A render-check refusal is
    asserted on its exact sentence (correction 3), unlike the schema shapes
    above.
    """
    result = render_overlay("tls:\n", tmp_path)
    assert result.returncode != 0, result.stdout
    assert (
        "`tls` is absent from the values, so `tls.enabled` cannot be read" in result.stderr
    ), result.stderr


def test_tls_not_a_map_is_refused_naming_tls_by_the_render_check(tmp_path: Path) -> None:
    result = render_overlay('tls: "x"\n', tmp_path)
    assert result.returncode != 0, result.stdout
    assert "`tls` must be a map and is string" in result.stderr, result.stderr


def test_tls_enabled_true_renders_successfully(tmp_path: Path) -> None:
    """`tls.enabled: true` is a shape this schema's own closure must accept:
    `type: boolean` and `required: [enabled]` (ADR-0845, C-DB1) bound the
    VALUE, never the value `true` itself, so turning TLS on must not trip
    anything this file owns.
    """
    result = render_overlay("tls:\n  enabled: true\n", tmp_path)
    assert result.returncode == 0, result.stderr


# ── RENDER: `tls.clientAuth` CARRIES NO DEFAULT (B-U5, ADR-0854) ───────────────────
#
# The schema refuses only ABSENCE (`required`), before any template runs (K-3
# accepts that pre-emption). `clientAuth` carries no `enum` and no `type`
# (ruling R1, ADR-0847): a bare `off`, a non-string and an unknown mode reach
# the render check's own named sentences, proved in `test_render_checks.py`
# against the real chart, schema validation on.


def assert_schema_refuses_client_auth(result: subprocess.CompletedProcess[str]) -> None:
    assert result.returncode != 0, result.stdout
    assert "values don't meet the specifications of the schema" in result.stderr, result.stderr
    found = extract_type_or_missing_refusal(result.stderr)
    assert found, result.stderr
    path, key = found
    assert path == ("tls",), (path, result.stderr)
    assert key == "clientAuth", (key, result.stderr)


def test_tls_client_auth_absent_is_refused_by_the_schema(tmp_path: Path) -> None:
    """`chart/ci/values.yaml` states `clientAuth: "off"`, so absence is made
    here by rendering WITHOUT that baseline — the same bare render an adopter
    who never set the key performs."""
    values = overlay("tls:\n  enabled: true\n", tmp_path)
    assert_schema_refuses_client_auth(
        helm("template", "task-db", str(CHART), "--values", str(values))
    )


def test_tls_client_auth_each_mode_passes_the_schema(tmp_path: Path) -> None:
    for mode in ("off", "optional", "required"):
        result = render_overlay(
            f'tls:\n  clientAuth: "{mode}"\n  clientCaSecret: peer-ca\n'
            "  clientCaSecretKey: ca.crt\n",
            tmp_path / mode,
        )
        assert result.returncode == 0, (mode, result.stderr)


def lint(chart: Path) -> subprocess.CompletedProcess[str]:
    binary = shutil.which("helm")
    assert binary
    return subprocess.run(
        [binary, "lint", "--strict", str(chart)], capture_output=True, text=True
    )


def test_a_bare_lint_refuses_the_missing_tls_enabled() -> None:
    """A bare `helm lint --strict .`, no `-f` at all — what an adopter who has
    not yet read `chart/ci/values.yaml` runs. `values.yaml` ships no default
    for `tls.enabled`, so this is the SHAPE OF THE SCHEMA'S OWN CONTRIBUTION:
    `templates/render-checks.yaml`'s `fail` logs as INFO under `lint` (helm
    grades a template `fail` as INFO, never ERROR — correction 1), so this
    case is red ONLY because the schema's `required` makes it an ERROR.
    `test_dropping_tls_required_degrades_the_bare_lint_message` is this
    test's own mutation check — the case stays red either way (see there for
    why), so what it proves is the MESSAGE this schema keyword buys.
    """
    result = lint(CHART)
    assert result.returncode != 0, result.stdout
    combined = result.stdout + result.stderr
    # THE STABLE WRAPPER, never the per-leaf phrase: measured identically on
    # 3.18.4, 3.20.2 and 4.3.0 (CI's `ci / precommit` pins 3.18.4 via
    # `azure/setup-helm@…`), while the phrase naming `enabled` itself takes
    # two different shapes across that range (`missing property 'enabled'`
    # vs `enabled is required`) — exactly the drift `extract_type_or_missing_
    # refusal` exists to absorb. Key and path are asserted through it rather
    # than through either literal sentence.
    assert "values don't meet the specifications of the schema" in combined, combined
    found = extract_type_or_missing_refusal(combined)
    assert found, combined
    path, key = found
    assert path == ("tls",), (path, combined)
    # EITHER of `tls`'s two required keys (B-U5 added `clientAuth`): a bare
    # lint is missing both, and which one the parser meets first is helm's
    # line order — 3.18.4 prints `enabled` inline after `[ERROR] values.yaml:`
    # and `clientAuth` on a line of its own, measured.
    assert key in ("enabled", "clientAuth"), (key, combined)
    assert "enabled" in combined, combined
    assert "clientAuth" in combined, combined


def test_dropping_tls_required_degrades_the_bare_lint_message(tmp_path: Path) -> None:
    """PROOF (card): drop the schema `required` and this stops being the
    SCHEMA's finding.

    MEASURED, not the simpler claim an earlier version of this test made:
    `helm lint --strict` does NOT go green. `templates/deployment.yaml`'s
    unconditional `ternary "1" "0" (and (kindIs "map" .Values.tls)
    .Values.tls.enabled)` still runs under `lint` (a template `fail` logs as
    INFO there and does not stop execution — correction 1's own point), and
    `ternary` itself refuses a `nil` condition with a raw sprig error —
    `invalid value; expected bool` — which is a genuine [ERROR], not an INFO
    line. So the case stays red either way; what `required` actually buys is
    the MESSAGE an adopter reads: a named sentence pointing at `tls.enabled`
    and `chart/values.yaml`, instead of a Go template path pointing at a
    line number in `deployment.yaml`. This is the finding C-DB1's sibling
    units (iam-db, project-db) measured identically on their own copy of
    this card; asserting red-stays-red here keeps the three repos making the
    same claim.

    A fresh copy of the WHOLE chart, because `-f` cannot replace
    `values.schema.json` itself — only editing the file on disk can.
    """
    copy = tmp_path / "chart"
    shutil.copytree(CHART, copy)
    schema = json.loads((copy / "values.schema.json").read_text())
    schema["properties"]["tls"]["required"] = []
    (copy / "values.schema.json").write_text(json.dumps(schema))

    result = lint(copy)
    assert result.returncode != 0, (
        "removing `required: [enabled]` from `tls` was expected to STAY red, for "
        f"a worse reason: {result.stdout}{result.stderr}"
    )
    combined = result.stdout + result.stderr
    # THE STABLE WRAPPER IS GONE, not a per-leaf phrase: once `required` is
    # dropped, the schema raises no finding about `tls` at all — proven by
    # the absence of the wrapper every schema violation carries, rather than
    # by the absence of either version's own wording for this one leaf.
    assert "values don't meet the specifications of the schema" not in combined, (
        f"the schema's own finding should be gone once `required` is dropped: {combined}"
    )
    assert "invalid value; expected bool" in combined, (
        f"expected the degraded sprig type error in its place: {combined}"
    )


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
        [binary, "lint", "--strict", str(CHART), "-f", str(CI_VALUES_PATH), "-f", str(values)],
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
        if path in REQUIRED_NO_DEFAULT:
            continue  # a third category: see REQUIRED_NO_DEFAULT's own comment.
        present, _ = value_at(values, path)
        if not present:
            undeclared_in_values.append(path)
    assert set(undeclared_in_values) == EXTRA_PATHS, undeclared_in_values


def test_every_required_no_default_key_has_no_default_in_values_yaml() -> None:
    """The other half of what REQUIRED_NO_DEFAULT claims: `values.yaml` really
    does not set it. A key that crept back into `values.yaml` would still pass
    the extras check above (it is skipped there, not merely tolerated), so
    this is the test that would catch it.
    """
    values = load_values()
    for path in REQUIRED_NO_DEFAULT:
        present, _ = value_at(values, path)
        assert not present, (
            f"{'.'.join(path)} is in REQUIRED_NO_DEFAULT but values.yaml sets it — "
            f"ADR-0845 asks for no default, not merely an unenforced one"
        )


def test_every_required_no_default_key_is_required_in_the_schema() -> None:
    """The schema half: a key with no default must be `required` by its
    parent block, or an adopter who omits it gets `nil`/zero rather than a
    refusal naming the key.
    """
    schema = load_schema()
    for path in REQUIRED_NO_DEFAULT:
        cursor = schema
        for step in path[:-1]:
            cursor = cursor["properties"][step]
        assert path[-1] in cursor.get("required", []), (
            f"{'.'.join(path)} is in REQUIRED_NO_DEFAULT but its parent block does "
            f"not list it under `required`"
        )


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
    assert "<root>" in violations, violations


def test_deleting_global_reddens_the_global_check() -> None:
    schema = load_schema()
    del schema["properties"]["global"]
    assert actual_open_map_paths(schema, load_values()) != OPEN_PATHS


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
