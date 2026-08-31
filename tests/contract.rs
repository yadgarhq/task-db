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

    w.db.update_task(Request::new(UpdateTaskRequest {
        scope: w.scope(P_A, U1),
        id: id.clone(),
        expect_version: 1,
        task: Some(Task {
            tags: vec!["added".into()],
            ..new_task("t")
        }),
        update_mask: None,
        idempotency: None,
    }))
    .await
    .expect("update");

    assert_eq!(
        w.read(P_A, U1, &id).await.expect("read").tags,
        vec!["added".to_string()]
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
