//! What a caller may reach: the project axis and the visibility axis, filtered
//! independently (D53's amendment to D12).
//!
//! Both axes are enforced in the `WHERE`, never after the fact. A row the caller
//! may not see must not be fetched and then dropped — the difference matters the
//! day someone logs the pre-filter result.

mod support;

use support::{two_projects_two_users, World, OTHER_TEAM, P_A, P_B, TEAM, U1, U2};
use yadgar_task_db::pb::yadgar::common::v1::Visibility;

// ---------------------------------------------------------------------------
// The project axis. `subtree()` builds a LIKE pattern out of a project id, and
// a project id is caller data reaching a pattern language.
// ---------------------------------------------------------------------------

/// `_` is a LIKE wildcard matching any single character, so an unescaped
/// `acme_team/%` matches `acmeXteam/secret` — a project nobody granted.
///
/// BOTH records belong to the SAME user on purpose. Seeding the sibling to a
/// second user would let the visibility ladder hide it, and the test would pass
/// with the escaping still broken.
#[tokio::test]
async fn an_underscore_in_a_project_id_is_not_a_wildcard() {
    let w = World::fresh("td_like_underscore").await;
    w.create("acme_team/x", U1, "mine").await;
    w.create("acmeXteam/secret", U1, "secret").await;

    let seen = w.list("acme_team", U1).await;
    let titles: Vec<_> = seen.iter().map(|t| t.title.as_str()).collect();

    assert_eq!(
        titles,
        vec!["mine"],
        "`_` must be escaped: a scope at acme_team reached acmeXteam"
    );
}

/// The same hole read at its widest. `%` matches everything, so a caller scoped
/// to the literal project `%` would be handed the entire table.
#[tokio::test]
async fn a_percent_in_a_project_id_is_not_a_wildcard() {
    let w = World::fresh("td_like_percent").await;
    w.create(P_A, U1, "a").await;
    w.create(P_B, U1, "b").await;

    assert!(
        w.list("%", U1).await.is_empty(),
        "`%` must be escaped: a scope of `%` returned the whole table"
    );
}

/// Backslash is the escape character itself, so it has to be escaped before the
/// other two or the escaping introduces its own wildcard.
#[tokio::test]
async fn a_backslash_in_a_project_id_matches_literally() {
    let w = World::fresh("td_like_backslash").await;
    w.create("acme\\x/one", U1, "mine").await;
    w.create("acme_x/two", U1, "other").await;

    let titles: Vec<_> = w
        .list("acme\\x", U1)
        .await
        .iter()
        .map(|t| t.title.clone())
        .collect();
    assert_eq!(titles, vec!["mine".to_string()]);
}

/// D53, unchanged by the escaping: an ancestor still reaches its descendants.
/// Escaping a pattern is easy to overdo into escaping the `/%` as well.
#[tokio::test]
async fn an_ancestor_still_reaches_its_descendants() {
    let w = World::fresh("td_like_subtree").await;
    w.create("acme/qwfm/forecast", U1, "deep").await;
    assert_eq!(w.list("acme/qwfm", U1).await.len(), 1);
}

// ---------------------------------------------------------------------------
// The visibility axis (D12). PRIVATE -> owner, TEAM -> members of team_id,
// ORG -> everyone. Every record below sits in ONE project, so nothing but the
// ladder can be doing the work.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn another_user_cannot_read_a_private_task() {
    let c = two_projects_two_users("td_vis_read").await;

    let err = c
        .read(P_A, U2, &c.u1_private)
        .await
        .expect_err("U2 must not read U1's PRIVATE task");
    assert_eq!(err.code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn another_user_cannot_list_a_private_task() {
    let c = two_projects_two_users("td_vis_list").await;

    let titles: Vec<_> = c
        .list(P_A, U2)
        .await
        .iter()
        .map(|t| t.title.clone())
        .collect();

    assert!(
        !titles.contains(&"u1 private".to_string()),
        "U1's PRIVATE task leaked into U2's list: {titles:?}"
    );
    assert!(
        titles.contains(&"u2 private".to_string()),
        "U2 must still see its own: {titles:?}"
    );
}

#[tokio::test]
async fn another_user_cannot_edit_a_private_task() {
    let c = two_projects_two_users("td_vis_edit").await;

    let err = c
        .edit_as(&c.scope(P_A, U2), &c.u1_private, "hijacked")
        .await
        .expect_err("U2 must not edit U1's PRIVATE task");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);

    let still = c.read(P_A, U1, &c.u1_private).await.expect("owner reads");
    assert_eq!(still.title, "u1 private", "the edit must not have landed");
}

/// The number arm of GetTask is a separate statement, and a filter added to one
/// arm and not the other is the shape this whole finding has.
#[tokio::test]
async fn the_number_arm_honours_visibility_too() {
    use tonic::Request;
    use yadgar_task_db::pb::yadgar::task::v1::task_db_service_server::TaskDbService as _;
    use yadgar_task_db::pb::yadgar::task::v1::{get_task_request, GetTaskRequest};

    let c = two_projects_two_users("td_vis_number").await;
    let number = c
        .read(P_A, U1, &c.u1_private)
        .await
        .expect("owner reads")
        .number;

    let err =
        c.db.get_task(Request::new(GetTaskRequest {
            scope: c.scope(P_A, U2),
            key: Some(get_task_request::Key::Number(number)),
        }))
        .await
        .expect_err("U2 must not reach U1's PRIVATE task by number either");
    assert_eq!(err.code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn a_teammate_sees_a_team_task() {
    let c = two_projects_two_users("td_vis_team_yes").await;

    let seen = c
        .read_as(&c.scope_in(P_A, U1, &[TEAM]), &c.u3_team)
        .await
        .expect("a member of the named team may read it");
    assert_eq!(seen.title, "u3 team");
}

/// Owned by U3, so an accidental `OR owner_user_id = ?` cannot make this pass.
#[tokio::test]
async fn a_non_teammate_does_not_see_a_team_task() {
    let c = two_projects_two_users("td_vis_team_no").await;

    let err = c
        .read_as(&c.scope_in(P_A, U1, &[OTHER_TEAM]), &c.u3_team)
        .await
        .expect_err("a member of another team must not read it");
    assert_eq!(err.code(), tonic::Code::NotFound);
}

/// An empty team list must render as "no TEAM arm", never as `IN ()` — which is
/// a syntax error — and never as "the arm is absent, so everything passes".
#[tokio::test]
async fn a_caller_in_no_teams_sees_no_team_tasks() {
    let c = two_projects_two_users("td_vis_team_none").await;

    let err = c
        .read(P_A, U1, &c.u3_team)
        .await
        .expect_err("a caller belonging to no team must not read a TEAM task");
    assert_eq!(err.code(), tonic::Code::NotFound);
}

/// `unwrap_or(1)` accepted `VISIBILITY_UNSPECIFIED`, so rows carrying 0 may
/// exist in a live store — and nothing noticed, because nothing read the column.
///
/// A ladder of three equality arms matches such a row on NONE of them, which
/// turns the fix for a leak into a quiet loss of access: unreadable by everyone
/// including its owner, while still looking perfectly present in the table. The
/// rung is therefore "not one of the wider two" rather than `= 1`, and migration
/// 5 heals the stored rows so the invariant holds in the table too.
#[tokio::test]
async fn a_row_with_an_unrecognised_visibility_falls_back_to_private() {
    let c = two_projects_two_users("td_vis_zero").await;
    c.set_visibility(&c.u1_private, 0, "").await;

    let owner = c
        .read(P_A, U1, &c.u1_private)
        .await
        .expect("the owner must not lose a record to a value nobody anticipated");
    assert_eq!(owner.title, "u1 private");

    let err = c
        .read(P_A, U2, &c.u1_private)
        .await
        .expect_err("and it must fall back to the RESTRICTIVE rung, not the open one");
    assert_eq!(err.code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn an_org_task_is_visible_to_everyone_in_the_project() {
    let c = two_projects_two_users("td_vis_org").await;

    let seen = c.read(P_A, U2, &c.u3_org).await.expect("ORG is everyone");
    assert_eq!(seen.title, "u3 org");
}

/// The two axes are independent (D53). ORG widens WHO, never WHERE.
#[tokio::test]
async fn org_visibility_does_not_cross_a_project_boundary() {
    let c = two_projects_two_users("td_vis_axes").await;
    c.promote(&c.u1_elsewhere, Visibility::Org, "").await;

    let err = c
        .read(P_A, U1, &c.u1_elsewhere)
        .await
        .expect_err("P_B is not under P_A, whatever the visibility says");
    assert_eq!(err.code(), tonic::Code::NotFound);
}

/// D26 keeps delete owner-only, which is narrower than the ladder. Widening
/// delete to "whoever can see it" would be the plausible-looking mistake while
/// implementing the ladder everywhere else.
#[tokio::test]
async fn delete_stays_owner_only_even_for_an_org_task() {
    use tonic::Request;
    use yadgar_task_db::pb::yadgar::task::v1::task_db_service_server::TaskDbService as _;
    use yadgar_task_db::pb::yadgar::task::v1::DeleteTaskRequest;

    let c = two_projects_two_users("td_vis_delete").await;

    let err =
        c.db.delete_task(Request::new(DeleteTaskRequest {
            scope: c.scope(P_A, U2),
            id: c.u3_org.clone(),
            expect_version: 1,
            idempotency: None,
        }))
        .await
        .expect_err("seeing a task is not permission to delete it (D26)");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
}
