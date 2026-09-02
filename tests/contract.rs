//! TaskDbService against a real MariaDB.
//!
//! The contract test proto-contract-design.md places in each `-db` repo. It
//! panics rather than skips without an engine, for the reason D69 gives: a suite
//! that quietly passes with nothing behind it is the failure it exists to catch.

mod support;

use support::{World, P_A, U1, U2};
use tonic::Request;
use yadgar_task_db::pb::yadgar::common::v1::{Meta, Visibility};
use yadgar_task_db::pb::yadgar::task::v1::task_db_service_server::TaskDbService as _;
use yadgar_task_db::pb::yadgar::task::v1::*;

fn new_task(title: &str) -> Task {
    Task {
        title: title.into(),
        body: "b".into(),
        status: TaskStatus::Open as i32,
        ..Default::default()
    }
}

#[tokio::test]
async fn numbers_are_per_project_and_start_at_one() {
    let w = World::fresh("td_number").await;
    for (project, expected) in [("acme/a", 1), ("acme/a", 2), ("acme/b", 1)] {
        let r = w.try_create(project, U1, "t").await.expect("create");
        assert_eq!(r.number, expected, "number is per-project, not global");
    }
}

/// D53: a project id is a hierarchical path, so an ancestor scope reaches its
/// descendants. Equality matching would return nothing here and look like an
/// empty store rather than a scoping bug.
#[tokio::test]
async fn an_ancestor_scope_sees_descendant_tasks() {
    let w = World::fresh("td_scope").await;
    w.create("acme/qwfm/forecast", U1, "deep").await;
    assert_eq!(
        w.list("acme/qwfm", U1).await.len(),
        1,
        "ancestor must reach the subtree"
    );
}

#[tokio::test]
async fn another_project_cannot_read_the_task() {
    let w = World::fresh("td_isolate").await;
    let id = w.create(P_A, U1, "secret").await;

    let err = w
        .read("other/z", U1, &id)
        .await
        .expect_err("a foreign project must not read this task");
    assert_eq!(err.code(), tonic::Code::NotFound);
}

/// D8: compare-and-set. A stale writer must be refused, not silently win.
#[tokio::test]
async fn a_stale_version_is_refused() {
    let w = World::fresh("td_cas").await;
    let id = w.create(P_A, U1, "t").await;

    w.edit_as(&w.scope(P_A, U1), &id, "changed")
        .await
        .expect("first update wins");
    let err = w
        .edit_as(&w.scope(P_A, U1), &id, "changed again")
        .await
        .expect_err("the second writer at version 1 is stale");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
}

/// D26: delete is owner-only and soft.
#[tokio::test]
async fn only_the_owner_may_delete_and_the_row_survives() {
    let w = World::fresh("td_delete").await;
    let id = w.create(P_A, U1, "t").await;

    let del = |user: &str| DeleteTaskRequest {
        scope: w.scope(P_A, user),
        id: id.clone(),
        expect_version: 1,
        idempotency: None,
    };

    let err =
        w.db.delete_task(Request::new(del(U2)))
            .await
            .expect_err("a non-owner must not delete");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);

    w.db.delete_task(Request::new(del(U1)))
        .await
        .expect("the owner may delete");

    let err = w
        .read(P_A, U1, &id)
        .await
        .expect_err("a soft-deleted task is not returned");
    assert_eq!(err.code(), tonic::Code::NotFound);
}

/// An unset page_size is 0 on the wire, which must not mean "return nothing" —
/// that presents as an empty store rather than as a caller that forgot a field.
#[tokio::test]
async fn an_unset_page_size_returns_a_default_page() {
    let w = World::fresh("td_page").await;
    for i in 0..3 {
        w.create(P_A, U1, &format!("t{i}")).await;
    }
    assert_eq!(w.list(P_A, U1).await.len(), 3);
}

/// Accepted by the contract, forwarded by the logic service, and dropped on the
/// floor: there were no columns, and `row_to_task` returned `Vec::new()` for
/// both. A field a caller can set and never read back is worse than one that
/// does not exist — it looks like it worked.
#[tokio::test]
async fn tags_and_links_round_trip() {
    let w = World::fresh("td_tags").await;
    let tags = vec!["b".to_string(), "a".to_string(), "b".to_string()];
    let links = vec!["yadgar:adr:12".to_string(), "yadgar:task:x".to_string()];

    let id =
        w.db.create_task(Request::new(CreateTaskRequest {
            scope: w.scope(P_A, U1),
            task: Some(Task {
                tags: tags.clone(),
                links: links.clone(),
                ..new_task("tagged")
            }),
            idempotency: None,
        }))
        .await
        .expect("create")
        .into_inner()
        .meta
        .expect("meta")
        .id;

    let read = w.read(P_A, U1, &id).await.expect("read");
    assert_eq!(read.tags, tags, "order and duplicates must survive");
    assert_eq!(read.links, links);

    // And on the list path, which assembles rows by a different statement.
    let listed = w.list(P_A, U1).await;
    assert_eq!(listed[0].tags, tags);
}

#[tokio::test]
async fn an_update_rewrites_tags_and_links() {
    let w = World::fresh("td_tags_update").await;
    let id = w.create(P_A, U1, "t").await;

    let write = |tags: Vec<String>, mask: Option<Vec<&str>>, version: u64| UpdateTaskRequest {
        scope: w.scope(P_A, U1),
        id: id.clone(),
        expect_version: version,
        task: Some(Task {
            tags,
            ..new_task("t")
        }),
        update_mask: mask.map(|paths| prost_types::FieldMask {
            paths: paths.into_iter().map(String::from).collect(),
        }),
        idempotency: None,
    };

    w.db.update_task(Request::new(write(
        vec!["added".into()],
        Some(vec!["tags"]),
        1,
    )))
    .await
    .expect("a mask that names tags writes them");
    assert_eq!(
        w.read(P_A, U1, &id).await.expect("read").tags,
        vec!["added".to_string()]
    );

    // A caller built against the older contract cannot populate `tags`, so its
    // empty vec is the field's zero value rather than an instruction. Treating
    // it as one would erase a task's tags on every status change made by a pod
    // that has not been upgraded yet.
    w.db.update_task(Request::new(write(vec![], None, 2)))
        .await
        .expect("an unmasked update is still allowed");
    assert_eq!(
        w.read(P_A, U1, &id).await.expect("read").tags,
        vec!["added".to_string()],
        "an unmasked update must not erase tags it never knew about"
    );
}

// ---------------------------------------------------------------------------
// D42: the caller supplies CONTENT, never identity. Every Meta field is
// assigned by the module.
// ---------------------------------------------------------------------------

/// Binding `meta.visibility` from the request let a caller publish a record to
/// the whole organisation, or — via the `unwrap_or(1)` default — persist
/// `VISIBILITY_UNSPECIFIED`, which common.proto says is never stored.
#[tokio::test]
async fn a_caller_supplied_identity_is_refused() {
    let w = World::fresh("td_meta_vis").await;

    for meta in [
        Meta {
            visibility: Visibility::Org as i32,
            ..Default::default()
        },
        Meta {
            team_id: "someone-elses-team".into(),
            ..Default::default()
        },
        Meta {
            owner_user_id: U2.into(),
            ..Default::default()
        },
        Meta {
            id: "yadgar:task:chosen-by-the-caller".into(),
            ..Default::default()
        },
    ] {
        let err =
            w.db.create_task(Request::new(CreateTaskRequest {
                scope: w.scope(P_A, U1),
                task: Some(Task {
                    meta: Some(meta.clone()),
                    ..new_task("t")
                }),
                idempotency: None,
            }))
            .await
            .expect_err("identity is assigned by the module, not supplied (D42)");
        assert_eq!(err.code(), tonic::Code::InvalidArgument, "for {meta:?}");
    }
}

/// The other half, and the one that actually catches the binding: what ends up
/// in the column. Refusing a populated Meta proves nothing if the accepted path
/// still takes the caller's word.
#[tokio::test]
async fn the_module_assigns_the_most_restrictive_visibility() {
    let w = World::fresh("td_meta_default").await;
    let id = w.create(P_A, U1, "t").await;

    assert_eq!(
        w.stored_visibility(&id).await,
        Visibility::Private as i8,
        "D12 defaults to the most restrictive rung, and it is never UNSPECIFIED"
    );
    assert_eq!(w.stored_team(&id).await, "");
}

/// An empty Meta is what the logic service actually sends, and it must stay the
/// ordinary case rather than tripping the guard above.
#[tokio::test]
async fn an_empty_meta_is_accepted() {
    let w = World::fresh("td_meta_empty").await;
    w.db.create_task(Request::new(CreateTaskRequest {
        scope: w.scope(P_A, U1),
        task: Some(Task {
            meta: Some(Meta::default()),
            ..new_task("t")
        }),
        idempotency: None,
    }))
    .await
    .expect("an empty Meta carries no identity and is fine");
}

/// The three fields the guard did not name. `Meta` carries `created_at`,
/// `updated_at` and `deleted_at`, and the check beside them listed the other
/// nine — so a caller could set any of the three and receive OK.
///
/// Nothing wrong reached the columns: `insert` never binds them and the engine
/// defaults them itself. That is what kept this invisible, and it is not a
/// defence. The caller asked for something, the module discarded it, and the
/// answer reported success — the same class as an identity a caller names for
/// itself, and the same reason D9's amendment refuses a request it would
/// otherwise silently drop.
///
/// The fixture's timestamp is one this module could not have produced: the
/// first day of the year 2000, long before any of these rows exist.
#[tokio::test]
async fn a_caller_supplied_timestamp_is_refused() {
    let w = World::fresh("td_meta_time").await;
    let impossible = prost_types::Timestamp {
        seconds: 946_684_800,
        nanos: 0,
    };

    for meta in [
        Meta {
            created_at: Some(impossible),
            ..Default::default()
        },
        Meta {
            updated_at: Some(impossible),
            ..Default::default()
        },
        Meta {
            deleted_at: Some(impossible),
            ..Default::default()
        },
    ] {
        let err =
            w.db.create_task(Request::new(CreateTaskRequest {
                scope: w.scope(P_A, U1),
                task: Some(Task {
                    meta: Some(meta.clone()),
                    ..new_task("stamped")
                }),
                idempotency: None,
            }))
            .await
            .expect_err("timestamps are assigned by this module, not supplied (D42)");
        assert_eq!(err.code(), tonic::Code::InvalidArgument, "for {meta:?}");
    }

    // The half a status code alone cannot prove: a refused create wrote
    // nothing, so no row carries the year 2000 either.
    assert_eq!(
        w.count_titled("stamped").await,
        0,
        "a refused create must leave no row behind"
    );
}

/// The same guard, on the OTHER write path. `CreateTask` refuses a caller-supplied
/// `Meta`; `UpdateTask` did not call the guard at all, so the request was answered
/// OK and the `Meta` was dropped on the floor.
///
/// **State the severity honestly: nothing wrong was stored.** `Field::ALL` is
/// title, body, status, tags and links, so no `Meta` field has a column an update
/// could reach. That is exactly the argument the create-path timestamp fix already
/// rejected — the caller asked for something, the module discarded it, and the
/// answer said success. This closes the same class on the same module.
///
/// Which is why asserting "the stored row does not contain the year 2000" would be
/// worthless here: it passes against the unfixed code too. The assertion that
/// carries weight is that the call is REFUSED and the task is UNTOUCHED — same
/// version, same title — which is what proves the refusal happened before the
/// write rather than after it.
///
/// The fixtures are values this module could not have produced: the first day of
/// the year 2000, and a version no row ever reaches because a create starts at 1
/// and every write increments by one.
#[tokio::test]
async fn a_caller_supplied_meta_on_update_is_refused() {
    let w = World::fresh("td_meta_update").await;
    let id = w.create(P_A, U1, "untouched").await;
    let impossible = prost_types::Timestamp {
        seconds: 946_684_800,
        nanos: 0,
    };

    for meta in [
        Meta {
            version: 9999,
            ..Default::default()
        },
        Meta {
            created_at: Some(impossible),
            ..Default::default()
        },
        Meta {
            updated_at: Some(impossible),
            ..Default::default()
        },
        Meta {
            deleted_at: Some(impossible),
            ..Default::default()
        },
        Meta {
            visibility: Visibility::Org as i32,
            ..Default::default()
        },
        Meta {
            owner_user_id: U2.into(),
            ..Default::default()
        },
        Meta {
            team_id: "someone-elses-team".into(),
            ..Default::default()
        },
        Meta {
            id: "yadgar:task:chosen-by-the-caller".into(),
            ..Default::default()
        },
    ] {
        let err =
            w.db.update_task(Request::new(UpdateTaskRequest {
                scope: w.scope(P_A, U1),
                id: id.clone(),
                expect_version: 1,
                task: Some(Task {
                    meta: Some(meta.clone()),
                    ..new_task("rewritten")
                }),
                update_mask: None,
                idempotency: None,
            }))
            .await
            .expect_err("meta is assigned by this module on every write path (D42)");
        assert_eq!(err.code(), tonic::Code::InvalidArgument, "for {meta:?}");
    }

    // The half a status code cannot prove on its own: the refusal came BEFORE the
    // statement, so the row still carries the title and the version it had. A
    // guard that ran after the UPDATE would leave version 2 here.
    let stored = w.read(P_A, U1, &id).await.expect("read");
    assert_eq!(stored.title, "untouched", "a refused update wrote nothing");
    assert_eq!(
        stored.meta.expect("meta").version,
        1,
        "a refused update must not have consumed the version"
    );
}

/// The counterpart, and the one that stops the guard breaking real traffic. An
/// absent `Meta` is what `task` actually sends — `writes::edit_request` and
/// `writes::transition_request` both build their `Task` from `..Default::default()`
/// — and an empty one is what any caller that populates the field will send.
/// Refusing either would turn a working path into `INVALID_ARGUMENT`.
#[tokio::test]
async fn an_absent_or_empty_meta_on_update_is_accepted() {
    let w = World::fresh("td_meta_update_ok").await;
    let id = w.create(P_A, U1, "t").await;

    for (version, meta) in [(1, None), (2, Some(Meta::default()))] {
        w.db.update_task(Request::new(UpdateTaskRequest {
            scope: w.scope(P_A, U1),
            id: id.clone(),
            expect_version: version,
            task: Some(Task {
                meta,
                ..new_task("edited")
            }),
            update_mask: None,
            idempotency: None,
        }))
        .await
        .expect("a Meta carrying no identity is the ordinary case");
    }
}
