# task-db

The `task` module's **`-db` twin**: the only writer of its store, serving
`TaskDbService` over gRPC. It holds no business rules — the boundary is the job.

Decisions in [`yadgarhq/docs`](https://github.com/yadgarhq/docs): D4 (the twin as
connection concentrator), D5 (one call, one transaction), D7 (capabilities, not
SQL dialects), D69 (the probe), D70 (how the protos get here).

## The protos are vendored, not fetched

`proto/` is a **subset** of [`yadgarhq/proto`](https://github.com/yadgarhq/proto),
exported at the tag in `PROTO_VERSION` for the packages in `PROTO_PATHS`. Buf
closes the import graph itself, so listing `yadgar/task/v1` also brings
`yadgar/common/v1`.

```bash
make proto      # refresh from the pin — the only sanctioned way to change proto/
```

CI re-runs that export and **fails on any difference**. Vendoring is normally skew
waiting to happen and is defensible here only because that check exists.

## Boot order is a decision, not wiring

Probe → migrate → serve, and the process does not listen until all three succeed.

A capability gap is a boot failure (D7), and the probe runs before the pool is
declared ready (D69) so a failure is a crash-loop rather than a pod that accepts
traffic and fails queries. Under D68 the second shape is worse than useless: a pod
that starts and then errors is one the autoscaler adds replicas around.

This module requires `transactions` and `row-locking` — **not** vector or
full-text. It is an addressed module (D10); requiring either would make it refuse
to boot on an engine that serves it perfectly well.

## Local development

```bash
podman run -d --name td -e MARIADB_ROOT_PASSWORD=probe -e MARIADB_DATABASE=probe \
  -p 3306:3306 mariadb:11.8
export YADGAR_TEST_DSN='mysql://root:probe@127.0.0.1:3306/probe'
cargo test
```

`protoc` must be on `PATH` — types are generated from the contract, never
hand-written (D16). The `rust-build` base image carries it; on NixOS,
`nix-shell -p protobuf`.

The tests **panic** rather than skip without `YADGAR_TEST_DSN`. A contract suite
that quietly passes with no engine behind it proves nothing, which is the state
this repository's dependency was in before D69.

## Configuration

| variable                                                        | required? — and what the chart renders                                                        |                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| --------------------------------------------------------------- | --------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `DB_HOST` / `DB_PORT` / `DB_NAME` / `DB_USER`                   | required — from `database.host` / `.port` / `.name` / `.user`                                 |                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `DB_PASSWORD_FILE`                                              | required — `/var/run/secrets/task-db/password`                                                | a mounted Secret the operator issued (D58) — never an env var                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| `DB_MAX_CONNECTIONS` / `REPLICAS` / `DB_ENGINE_MAX_CONNECTIONS` | required — from `database.maxConnections`, the replica count, `database.engineMaxConnections` | the product is checked at boot and refused if it would exhaust the engine (D4)                                                                                                                                                                                                                                                                                                                                                                                                                               |
| `DB_SSL_CA_FILE`                                                | optional — rendered only when `database.sslCaSecret` names a Secret                           | the certificate authority `verify_identity` checks the engine's certificate against. Unset means sqlx's own trust store — the PUBLIC web roots, which sign no operator-issued, RDS or Aurora certificate — so `verify_identity` against a privately-signed engine is refused without this. Setting it WIDENS the trust set rather than replacing it: sqlx seeds the public roots first and appends this file, which is why a CA alone never made `verify_ca` safe. Inert under the three non-verifying modes |
| `DB_SSL_MODE`                                                   | required — from `database.sslMode`                                                            | how TLS is negotiated to the engine, for BOTH D7's boot probe and the serving pool: `disabled`, `preferred`, `required`, `verify_identity`. An unrecognised value refuses the boot, and so does a `DB_REQUIRE_TLS` left over from before this key replaced it. `verify_ca` still PARSES — the refusal above lists it — but no connection can be built with it: it named a check sqlx does not perform, so `store` refuses it at boot and the process exits naming `verify_identity`                          |
| `LISTEN`                                                        | required — `0.0.0.0:50051`, matching the chart's `containerPort`                              |                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |

## The Service is headless, deliberately

A normal Service balances at L4, and a gRPC client holds one long-lived HTTP/2
connection — so it would pin to a single pod and leave the rest idle.
`clusterIP: None` publishes every pod address and the client balances across them
(D23).
