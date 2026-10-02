## What

`Readable::bind`'s sentinel paragraph in `src/sql.rs` named two tests that go
red when `Reach::bind_visible`'s `self.user` binding is replaced by a literal,
with no verified total. This corrects it with a measured count and the method
used to measure it.

## Why

Ledger 746: the paragraph's sibling sentinel (the one above it, for the
`owner_user_id` arm) carries a verified total ("and six tests with it"); this
one did not, even though the evidence it was built from exists the same way.
An unverified "two tests go red" reads as a claim nobody checked, next to one
that visibly was. This is a documentation-only fix — no production code
changes.

## Changelog

- docs(sql): record the measured 16-test count for bind_visible's mutation sentinel

## Verification

Ran the mutation by hand: replaced `query.bind(&self.user)` in
`Reach::bind_visible` with `query.bind("mutant-sentinel-literal")`, confirmed
the whole suite was green beforehand, then ran `cargo test --no-fail-fast`
against `mariadb:11.8` (`podman run mariadb:11.8`, `YADGAR_TEST_DSN` per
README). 16 tests went red: 3 in `tests/contract.rs`, 4 in
`tests/idempotency.rs`, 1 in `tests/known_answer.rs`, 1 in `tests/list.rs`, 1
in `tests/migration.rs`, 4 in `tests/previous_status.rs`, 2 in
`tests/scope.rs`. Reverted the mutation (`git diff` against `origin/main`
confirmed `src/sql.rs` matched exactly before the comment edit), then ran
`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and the full
`cargo test` suite clean — all pass, 0 failed.

## Risk

None. The diff touches only a doc comment; no executable code changed.
