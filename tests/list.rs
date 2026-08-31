//! `ListTasks`: the status filter, the page, and the token that walks it.
//!
//! A filter the service accepts and discards is worse than one it refuses — the
//! caller believes it narrowed the read, and pages through the whole table
//! thinking it asked for four rows.

mod support;

use std::collections::HashSet;

use support::{World, P_A, U1};
use tonic::Request;
use yadgar_task_db::pb::yadgar::task::v1::task_db_service_server::TaskDbService as _;
use yadgar_task_db::pb::yadgar::task::v1::*;

async fn page(w: &World, statuses: Vec<i32>, page_size: i32, token: &str) -> ListTasksResponse {
    w.db.list_tasks(Request::new(ListTasksRequest {
        scope: w.scope(P_A, U1),
        statuses,
        page_size,
        page_token: token.into(),
    }))
    .await
    .expect("list")
    .into_inner()
}

async fn set_status(w: &World, id: &str, version: u64, status: TaskStatus) {
    w.db.update_task(Request::new(UpdateTaskRequest {
        scope: w.scope(P_A, U1),
        id: id.into(),
        expect_version: version,
        task: Some(Task {
            title: "t".into(),
            body: "b".into(),
            status: status as i32,
            ..Default::default()
        }),
        update_mask: None,
        idempotency: None,
    }))
    .await
    .expect("status");
}

#[tokio::test]
async fn statuses_narrow_the_result() {
    let w = World::fresh("td_list_status").await;
    let open = w.create(P_A, U1, "open").await;
    let done = w.create(P_A, U1, "done").await;
    let blocked = w.create(P_A, U1, "blocked").await;
    set_status(&w, &done, 1, TaskStatus::Done).await;
    set_status(&w, &blocked, 1, TaskStatus::Blocked).await;

    let only_done = page(&w, vec![TaskStatus::Done as i32], 0, "").await;
    let ids: Vec<_> = only_done
        .tasks
        .iter()
        .map(|t| t.meta.as_ref().expect("meta").id.clone())
        .collect();
    assert_eq!(ids, vec![done.clone()], "the status filter was discarded");

    let two = page(
        &w,
        vec![TaskStatus::Done as i32, TaskStatus::Blocked as i32],
        0,
        "",
    )
    .await;
    assert_eq!(
        two.tasks.len(),
        2,
        "a repeated field means OR, not the first"
    );

    let none = page(&w, vec![TaskStatus::InProgress as i32], 0, "").await;
    assert!(none.tasks.is_empty());

    assert_eq!(
        page(&w, vec![], 0, "").await.tasks.len(),
        3,
        "an empty filter means every status, not none"
    );
    assert!(!open.is_empty());
}

#[tokio::test]
async fn an_unknown_status_is_refused_rather_than_matching_nothing() {
    let w = World::fresh("td_list_bad_status").await;
    let err =
        w.db.list_tasks(Request::new(ListTasksRequest {
            scope: w.scope(P_A, U1),
            statuses: vec![99],
            page_size: 0,
            page_token: String::new(),
        }))
        .await
        .expect_err("99 is not a TaskStatus");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

/// The whole set, once each, in pages. `next_page_token` is always `""` today,
/// so a caller asking for a second page gets the FIRST one again — an infinite
/// loop that looks like an enormous table.
#[tokio::test]
async fn a_token_walks_every_page_exactly_once() {
    let w = World::fresh("td_list_pages").await;
    for i in 0..5 {
        w.create(P_A, U1, &format!("t{i}")).await;
    }

    let mut seen: Vec<String> = Vec::new();
    let mut token = String::new();
    for _ in 0..10 {
        let p = page(&w, vec![], 2, &token).await;
        assert!(p.tasks.len() <= 2, "a page must honour page_size");
        seen.extend(
            p.tasks
                .iter()
                .map(|t| t.meta.as_ref().expect("meta").id.clone()),
        );
        token = p.next_page_token;
        if token.is_empty() {
            break;
        }
    }

    assert!(token.is_empty(), "the walk never terminated");
    assert_eq!(seen.len(), 5, "pages did not cover the set exactly once");
    assert_eq!(
        seen.iter().collect::<HashSet<_>>().len(),
        5,
        "a row repeated"
    );
}

/// The last page must say so. A token returned on an exhausted page costs the
/// caller one more round trip and, worse, reads as "there is more".
#[tokio::test]
async fn an_exact_final_page_returns_no_token() {
    let w = World::fresh("td_list_exact").await;
    for i in 0..4 {
        w.create(P_A, U1, &format!("t{i}")).await;
    }

    let first = page(&w, vec![], 2, "").await;
    assert!(!first.next_page_token.is_empty());
    let second = page(&w, vec![], 2, &first.next_page_token).await;
    assert_eq!(second.tasks.len(), 2);
    assert!(
        second.next_page_token.is_empty(),
        "four rows in pages of two is two pages, not three"
    );
}

/// D56 bounds reads. An unbounded page is how one caller takes the whole table.
#[tokio::test]
async fn a_page_size_above_the_ceiling_is_clamped() {
    let w = World::fresh("td_list_clamp").await;
    w.seed_rows(P_A, U1, 501, TaskStatus::Open).await;

    let p = page(&w, vec![], 10_000, "").await;
    assert_eq!(p.tasks.len(), 500, "page_size must be clamped to 500");
    assert!(
        !p.next_page_token.is_empty(),
        "a clamped page is not the last page"
    );
}

#[tokio::test]
async fn a_token_from_a_narrower_scope_cannot_widen_it() {
    let w = World::fresh("td_list_token_scope").await;
    w.create(P_A, U1, "mine").await;
    let elsewhere = w.create("other/z", U1, "theirs").await;

    // A token is an id, and a caller can invent one. The WHERE still decides.
    let p = page(
        &w,
        vec![],
        50,
        "yadgar:task:00000000-0000-0000-0000-000000000000",
    )
    .await;
    let ids: Vec<_> = p
        .tasks
        .iter()
        .map(|t| t.meta.as_ref().expect("meta").id.clone())
        .collect();
    assert!(
        !ids.contains(&elsewhere),
        "a token must not widen the scope"
    );
}
