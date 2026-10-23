//! The `number` arm of `GetTask` resolves in the caller's EXACT project.
//!
//! `EditTask` and `TransitionTask` now accept a number (yaadgaar #397,
//! proto v1.22.0), and `task` resolves it through this arm before it writes. So
//! this arm is where `task` takes the rule "a number never resolves across
//! projects" from: a sibling's task, a descendant's, or an ancestor's must be
//! NOT_FOUND by number. The id arm keeps its subtree reach (D53), and the
//! ancestor-scope case below asserts both halves so the asymmetry is pinned
//! rather than assumed.

mod support;

use tonic::{Code, Request};

use support::{World, P_A, P_B, U1};
use yadgar_task_db::pb::yadgar::task::v1::task_db_service_server::TaskDbService as _;
use yadgar_task_db::pb::yadgar::task::v1::{get_task_request, GetTaskRequest};

/// The ancestor of both fixture projects. `P_A` is `acme/a`.
const ANCESTOR: &str = "acme";

async fn by_number(w: &World, project: &str, number: u32) -> Result<String, tonic::Status> {
    w.db.get_task(Request::new(GetTaskRequest {
        scope: w.scope(project, U1),
        key: Some(get_task_request::Key::Number(number)),
    }))
    .await
    .map(|r| r.into_inner().task.expect("task").title)
}

/// A number held only by a SIBLING project is NOT_FOUND, and the same number
/// in the caller's own project is found — so the NOT_FOUND is about the
/// project, not about the number.
#[tokio::test]
async fn a_number_held_only_by_a_sibling_project_is_not_found() {
    let w = World::fresh("td_number_sibling").await;
    w.create(P_B, U1, "in the sibling").await;

    let err = by_number(&w, P_A, 1)
        .await
        .expect_err("task 1 exists only in the sibling project");
    assert_eq!(err.code(), Code::NotFound);

    w.create(P_A, U1, "in the caller's project").await;
    assert_eq!(
        by_number(&w, P_A, 1).await.expect("now P_A holds a task 1"),
        "in the caller's project"
    );
}

/// From an ANCESTOR scope, a descendant's task is reachable by id (the
/// subtree, D53) and NOT by number: the number arm matches the project by
/// equality, so it cannot pick one of several descendants' task 1.
#[tokio::test]
async fn an_ancestor_scope_reaches_a_descendant_task_by_id_but_not_by_number() {
    let w = World::fresh("td_number_ancestor").await;
    let id = w.create(P_A, U1, "in the descendant").await;

    let by_id = w
        .read(ANCESTOR, U1, &id)
        .await
        .expect("the id arm reaches the subtree");
    assert_eq!(by_id.number, 1);

    let err = by_number(&w, ANCESTOR, 1)
        .await
        .expect_err("the number arm does not reach the subtree");
    assert_eq!(err.code(), Code::NotFound);
}

/// From a DESCENDANT scope, an ancestor's task is NOT_FOUND by number, and the
/// same number in the caller's own project is found.
///
/// The other direction of the ancestor test above. A prefix match written the
/// wrong way round (`? LIKE CONCAT(project_id, '%')`) reaches UP the tree, so
/// it passes the sibling and ancestor-scope tests and fails only here.
#[tokio::test]
async fn a_descendant_scope_does_not_reach_an_ancestor_task_by_number() {
    let w = World::fresh("td_number_descendant").await;
    w.create(ANCESTOR, U1, "in the ancestor").await;

    let err = by_number(&w, P_A, 1)
        .await
        .expect_err("task 1 exists only in the ancestor project");
    assert_eq!(err.code(), Code::NotFound);

    w.create(P_A, U1, "in the caller's project").await;
    assert_eq!(
        by_number(&w, P_A, 1).await.expect("now P_A holds a task 1"),
        "in the caller's project"
    );
}
