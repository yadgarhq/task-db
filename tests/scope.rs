//! What a caller may reach: the project axis and the visibility axis, filtered
//! independently (D53's amendment to D12).
//!
//! Both axes are enforced in the `WHERE`, never after the fact. A row the caller
//! may not see must not be fetched and then dropped — the difference matters the
//! day someone logs the pre-filter result.

mod support;

use support::{
    ladder_only, setting, shipped_setting, two_projects_two_users, World, OTHER_TEAM, P_A, P_B,
    TEAM, U1, U2, U3,
};
use yadgar_task_db::pb::yadgar::common::v1::{SettingValue, Visibility};

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

/// THE OWNER'S OWN TEAM RECORD, which nothing here reached.
///
/// Every other TEAM test above queries as U1 against a record owned by U3 —
/// deliberately, so an accidental `OR owner_user_id = ?` cannot carry them. The
/// consequence is that the owner's own path through the TEAM arm was never
/// walked by anything, and the arm most people would reach for to "fix" that is
/// the one `Reach::visible` documents as refusing.
///
/// U3 owns `u3_team` and belongs to `TEAM`, so the TEAM rung is what grants the
/// read. The PRIVATE rung does not and must not: `visibility NOT IN (2, 3)`
/// excludes a TEAM row whoever owns it.
#[tokio::test]
async fn the_owner_of_a_team_task_reads_it_through_the_team_arm() {
    let c = two_projects_two_users("td_vis_team_owner").await;

    let seen = c
        .read_as(&c.scope_in(P_A, U3, &[TEAM]), &c.u3_team)
        .await
        .expect("the owner, in the named team, may read their own TEAM record");
    assert_eq!(seen.title, "u3 team");
}

/// **THE ENFORCEMENT LANDING. `task-db#29` WROTE THIS TEST INVERTED AND SAID
/// SO.** It pinned the PRE-ENFORCEMENT behaviour — an owner outside the team of
/// their own `TEAM` record was refused — and its doc comment ruled in advance
/// that "when `resolve()` is wired into `Reach`, THIS TEST FAILING IS THE
/// ENFORCEMENT LANDING, NOT A REGRESSION. Invert it there… and keep the
/// two-half shape". This is that inversion.
///
/// The setting is the one `iam-db` migration 12 actually seeds: `ON`, locked.
/// ADR-0522's defect is the owner who LEFT the team their record is shared
/// with, and U3 here belongs to no team at all while still owning `u3_team`.
///
/// **THE TWO HALVES SURVIVE, WITH OWNERSHIP AS THE VARIABLE RATHER THAN
/// MEMBERSHIP.** A success is not vacuous the way `NOT_FOUND` is, but it can
/// still pass for the wrong reason — an arm that widened the row to EVERYBODY
/// satisfies half one exactly as well. So the same record is read twice under
/// the SAME setting and the SAME empty team list, and the only thing that
/// changes is who is asking: the owner reaches it, a non-owner does not.
///
/// **A BLANKET `OR owner_user_id = ?` IS STILL THE WRONG WAY TO GET HERE, and
/// this test does not distinguish it — deliberately, because a later test in
/// this file does.** Under `ON` and locked the setting resolves the same answer
/// for every team, so a blanket arm selects exactly these rows; it is an
/// EQUIVALENT mutant under this configuration. The tests that state `OFF`, and
/// the one that states a team override, are where it dies.
#[tokio::test]
async fn an_owner_outside_the_team_reaches_their_own_team_record() {
    let c = two_projects_two_users("td_vis_team_owner_left").await;
    let policy = Some(shipped_setting());

    // Half one: the owner who has left. The PRIVATE rung is
    // `visibility NOT IN (2, 3)`, which excludes a TEAM row whoever owns it,
    // and with no team list the TEAM arm is not rendered at all — so ADR-0522's
    // arm is the only thing in the statement that can be returning this row.
    let seen = c
        .read_as(&c.scope_with(P_A, U3, &[], policy.clone()), &c.u3_team)
        .await
        .expect("with the setting ON, an owner reads their own record from outside its team");
    assert_eq!(seen.title, "u3 team");

    // Half two: the same record, the same setting, the same empty team list,
    // and a caller who does not own it. The arm is keyed on ownership, so this
    // must still be refused — otherwise half one is passing because the row
    // became visible to everybody.
    let err = c
        .read_as(&c.scope_with(P_A, U1, &[], policy), &c.u3_team)
        .await
        .expect_err("the setting widens the OWNER's reach, not everyone's");
    assert_eq!(err.code(), tonic::Code::NotFound);
}

/// The same record and the same caller under `OFF`, which is `task-db#29`'s
/// original assertion with the policy now stated rather than absent.
///
/// **THIS IS WHERE A BLANKET `OR owner_user_id = ?` DIES.** An arm that ignores
/// the setting returns `u3_team` to U3 here, and no other test in this file can
/// see that: every other TEAM test queries as U1 or U2 against a record owned
/// by U3, deliberately, so `owner_user_id = <caller>` is false in all of them.
///
/// The two-half shape is `task-db#29`'s, unchanged, because `NOT_FOUND` is
/// still the cheapest vacuous pass in this suite: U3 reads `u3_team`
/// successfully while carrying `TEAM`, then fails to read the SAME record with
/// an empty team list. Team membership is the only variable between the calls.
#[tokio::test]
async fn a_setting_stating_off_leaves_an_owner_outside_the_team_where_they_were() {
    let c = two_projects_two_users("td_vis_team_owner_off").await;
    let policy = Some(setting(SettingValue::Off, true, &[]));

    let seen = c
        .read_as(&c.scope_with(P_A, U3, &[TEAM], policy.clone()), &c.u3_team)
        .await
        .expect("the owner in the named team reads their own TEAM record through the TEAM arm");
    assert_eq!(seen.title, "u3 team");

    let err = c
        .read_as(&c.scope_with(P_A, U3, &[], policy), &c.u3_team)
        .await
        .expect_err(
            "with the setting OFF, ownership alone does not reach a TEAM record — \
             a blanket `OR owner_user_id = ?` arm is what this refuses",
        );
    assert_eq!(err.code(), tonic::Code::NotFound);
}

// ---------------------------------------------------------------------------
// ADR-0522, enforced. `yadgar/common/v1` admits exactly two states for a `-db`
// and this service is now in the second: "A -db that reads it is in the
// ENFORCING state and the refusal binds it absolutely." Everything below is
// that refusal, and the arm it gates.
// ---------------------------------------------------------------------------

/// **THE REFUSAL, ON THE VERB THAT CARRIES IT.** An absent setting names no
/// policy, and the contract forbids a store choosing one: "There is no third
/// state in which a -db reads the field and tolerates it being unset."
///
/// This is the request every caller sent until `task` v0.4.3, so it is also the
/// assertion that makes the roll-out order real rather than advisory.
#[tokio::test]
async fn a_read_carrying_no_setting_is_refused_rather_than_answered() {
    let c = two_projects_two_users("td_setting_absent_get").await;

    let err = c
        .read_as(&c.scope_with(P_A, U1, &[], None), &c.u1_private)
        .await
        .expect_err("an absent setting names no policy, and a store may not pick one");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

/// The refusal reaches `ListTasks` too. A setting honoured on one read verb and
/// not the other is a caller who cannot fetch a record it can list, and the
/// two verbs build their statements separately — which is exactly the shape
/// that put a visibility filter on one `GetTask` arm and not the other.
#[tokio::test]
async fn a_list_carrying_no_setting_is_refused_rather_than_answered() {
    use yadgar_task_db::pb::yadgar::task::v1::task_db_service_server::TaskDbService as _;

    let c = two_projects_two_users("td_setting_absent_list").await;

    let err =
        c.db.list_tasks(tonic::Request::new(
            yadgar_task_db::pb::yadgar::task::v1::ListTasksRequest {
                scope: c.scope_with(P_A, U1, &[], None),
                statuses: vec![],
                page_size: 0,
                page_token: String::new(),
            },
        ))
        .await
        .expect_err("ListTasks reads the ladder, so it reads the setting");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

/// The number arm of `GetTask` is a THIRD statement, built from
/// `Reach::readable` rather than `Reach::read_predicate`, and a setting applied
/// to one arm and not the other is precisely the shape of the leak this file
/// was written for.
#[tokio::test]
async fn the_number_arm_carries_adr_0522_as_well() {
    use tonic::Request;
    use yadgar_task_db::pb::yadgar::task::v1::task_db_service_server::TaskDbService as _;
    use yadgar_task_db::pb::yadgar::task::v1::{get_task_request, GetTaskRequest};

    let c = two_projects_two_users("td_setting_number_arm").await;
    let number = c
        .read_as(
            &c.scope_with(P_A, U3, &[TEAM], Some(shipped_setting())),
            &c.u3_team,
        )
        .await
        .expect("the owner in the team reads their own record")
        .number;

    // Refused outright when the setting is absent.
    let err =
        c.db.get_task(Request::new(GetTaskRequest {
            scope: c.scope_with(P_A, U3, &[], None),
            key: Some(get_task_request::Key::Number(number)),
        }))
        .await
        .expect_err("the number arm refuses an unstated setting like the id arm does");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    // And widened by it when stated ON, from outside the team.
    let seen =
        c.db.get_task(Request::new(GetTaskRequest {
            scope: c.scope_with(P_A, U3, &[], Some(shipped_setting())),
            key: Some(get_task_request::Key::Number(number)),
        }))
        .await
        .expect("the number arm honours ADR-0522 too")
        .into_inner()
        .task
        .expect("task");
    assert_eq!(seen.title, "u3 team");
}

/// The OTHER wire form of "nothing stated". The two are distinguishable on the
/// wire, which is exactly why the contract says they are treated alike: "AN
/// ABSENT MESSAGE AND A PRESENT ONE HOLDING SETTING_VALUE_UNSPECIFIED ARE ONE
/// CASE, NOT TWO, AND BOTH ARE REFUSED."
#[tokio::test]
async fn a_present_setting_stating_nothing_is_refused_like_an_absent_one() {
    let c = two_projects_two_users("td_setting_zero_get").await;

    let err = c
        .read_as(
            &c.scope_with(
                P_A,
                U1,
                &[],
                Some(setting(SettingValue::Unspecified, false, &[])),
            ),
            &c.u1_private,
        )
        .await
        .expect_err("a present message holding the zero still names no policy");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

/// **THE ORDER OF THE RESOLUTION, VISIBLE FROM THE REQUEST PATH.** The
/// organisation states nothing and the RECORD'S OWN team states ON. An
/// implementation that consults the lock or the map before `org_value` finds an
/// override, answers ON, and refuses nothing — so the refusal is unreachable in
/// precisely the deployment that has an unconfigured organisation and a team
/// with an opinion.
///
/// The override is on `TEAM`, which is `u3_team`'s own team, on purpose:
/// written with an empty map or an override on some other team, this is
/// satisfied by the wrong-order implementation too and pins nothing.
#[tokio::test]
async fn an_override_does_not_rescue_a_read_whose_organisation_stated_nothing() {
    let c = two_projects_two_users("td_setting_precedence").await;

    let err = c
        .read_as(
            &c.scope_with(
                P_A,
                U3,
                &[],
                Some(setting(
                    SettingValue::Unspecified,
                    false,
                    &[(TEAM, SettingValue::On)],
                )),
            ),
            &c.u3_team,
        )
        .await
        .expect_err("the refusal is the FIRST step of the resolution, not a check beside it");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

/// **THE TEAM IS THE RECORD'S, NEVER THE CALLER'S, AND HERE THE CALLER HAS
/// NONE.** The organisation states OFF and unlocked; only `u3_team`'s own team
/// says ON. U3 belongs to no team at all, so an implementation that keyed the
/// override on `Scope.team_ids` finds nothing and answers OFF — which is the
/// setting evaporating in exactly the case ADR-0522 exists for.
#[tokio::test]
async fn an_override_on_the_records_team_reaches_an_owner_who_is_in_no_team() {
    let c = two_projects_two_users("td_setting_record_team").await;

    let seen = c
        .read_as(
            &c.scope_with(
                P_A,
                U3,
                &[],
                Some(setting(
                    SettingValue::Off,
                    false,
                    &[(TEAM, SettingValue::On)],
                )),
            ),
            &c.u3_team,
        )
        .await
        .expect("the record's own team states ON, whatever the caller belongs to");
    assert_eq!(seen.title, "u3 team");
}

/// The same policy with the override moved to a team the record is NOT in.
/// Absence in the map is how a team states nothing, so the organisation's OFF
/// stands — and an implementation that rendered the arm without matching the
/// row's `team_id` returns the record here.
#[tokio::test]
async fn an_override_on_another_team_does_not_reach_this_records_owner() {
    let c = two_projects_two_users("td_setting_other_team").await;

    let err = c
        .read_as(
            &c.scope_with(
                P_A,
                U3,
                &[],
                Some(setting(
                    SettingValue::Off,
                    false,
                    &[(OTHER_TEAM, SettingValue::On)],
                )),
            ),
            &c.u3_team,
        )
        .await
        .expect_err("the override belongs to another team; this record's team states nothing");
    assert_eq!(err.code(), tonic::Code::NotFound);
}

/// **THE EXCLUSION RENDERING, which is a DIFFERENT statement rather than the
/// inclusion one inverted.** The organisation states ON and unlocked, and the
/// record's own team says OFF — so the arm must SUBTRACT that team from an
/// otherwise blanket reach. A projection that only ever emitted an inclusion
/// list renders "only t-platform reaches", the exact complement of the policy,
/// and returns this record.
#[tokio::test]
async fn an_override_stating_off_subtracts_its_team_from_an_otherwise_open_setting() {
    let c = two_projects_two_users("td_setting_subtract").await;
    let policy = Some(setting(
        SettingValue::On,
        false,
        &[(TEAM, SettingValue::Off)],
    ));

    let err = c
        .read_as(&c.scope_with(P_A, U3, &[], policy.clone()), &c.u3_team)
        .await
        .expect_err("this record's team states OFF, so its owner does not reach it from outside");
    assert_eq!(err.code(), tonic::Code::NotFound);

    // The second half proves the subtraction is a subtraction rather than the
    // whole arm going missing: the SAME caller under the SAME setting still
    // reaches a record whose team the override does not name. `u1_private` is
    // U1's, so this is U1 reading their own PRIVATE record — which the ladder
    // grants anyway, and which therefore cannot be what fails.
    let seen = c
        .read_as(&c.scope_with(P_A, U1, &[], policy), &c.u1_private)
        .await
        .expect("a record whose team the override does not name is unaffected");
    assert_eq!(seen.title, "u1 private");
}

/// The lock is what makes an organisation's policy inescapable, and a
/// contradicting override must be IGNORED rather than merged. An
/// implementation that merely prefers an override where one exists refuses
/// this read.
#[tokio::test]
async fn a_locked_organisation_ignores_an_override_on_the_records_own_team() {
    let c = two_projects_two_users("td_setting_locked").await;

    let seen = c
        .read_as(
            &c.scope_with(
                P_A,
                U3,
                &[],
                Some(setting(
                    SettingValue::On,
                    true,
                    &[(TEAM, SettingValue::Off)],
                )),
            ),
            &c.u3_team,
        )
        .await
        .expect("a locked organisation ignores every override rather than merging with it");
    assert_eq!(seen.title, "u3 team");
}

/// `ListTasks` widens with the same setting, and it is a separate statement.
#[tokio::test]
async fn a_list_carries_the_owners_own_team_record_when_the_setting_states_on() {
    let c = two_projects_two_users("td_setting_list_widens").await;

    let titles: Vec<_> = c
        .list_as(&c.scope_with(P_A, U3, &[], Some(shipped_setting())))
        .await
        .iter()
        .map(|t| t.title.clone())
        .collect();

    assert!(
        titles.contains(&"u3 team".to_string()),
        "the owner's own TEAM record must appear in their list: {titles:?}"
    );
    assert!(
        !titles.contains(&"u1 private".to_string()),
        "and the arm must widen the OWNER's reach only: {titles:?}"
    );
}

/// **THE SETTING IS NAMED `owner_reads_own_record`, AND THE EDIT PATH IS
/// DELIBERATELY NOT WIDENED BY IT.** `UpdateTask` builds its statement from
/// `Reach::predicate`, which carries no ADR-0522 arm, so an absent setting does
/// not refuse a write and a stated one does not grant edit authority a
/// read-named setting never promised.
///
/// Both halves are asserted because either alone is satisfiable by an accident:
/// that the write is not REFUSED, and that it is not WIDENED.
#[tokio::test]
async fn the_edit_path_neither_refuses_nor_widens_on_this_setting() {
    let c = two_projects_two_users("td_setting_edit_untouched").await;

    // Not refused: an absent setting is fine for a write, which is what keeps
    // the enforcement blast radius to the two read verbs.
    c.edit_as(&c.scope_with(P_A, U1, &[], None), &c.u1_private, "renamed")
        .await
        .expect("an absent setting must not refuse an edit; the setting names reads");

    // Not widened: U3 owns `u3_team` and is in no team, so ADR-0522 lets them
    // READ it — and the edit path still refuses, under the very setting that
    // grants the read.
    let policy = Some(shipped_setting());
    c.read_as(&c.scope_with(P_A, U3, &[], policy.clone()), &c.u3_team)
        .await
        .expect("the read is granted, which is what makes the refusal below attributable");

    let err = c
        .edit_as(&c.scope_with(P_A, U3, &[], policy), &c.u3_team, "hijacked")
        .await
        .expect_err("a setting named for reads must not hand out edit authority");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
}

/// **THE ADVICE IN A REFUSAL MUST BE FOLLOWABLE, AND ENFORCEMENT MADE THIS ONE
/// A LOOP.** `UpdateTask` said "re-read and retry". Before `task-db#30` that was
/// consistent: an owner outside the team of their own `TEAM` record could not
/// read it either, so the re-read failed and the caller stopped. ADR-0522 grants
/// the read and deliberately does not widen the edit, so the re-read now
/// SUCCEEDS and returns the same version — and a caller following the advice
/// sends the identical request forever.
///
/// The loop below is characterization: it passes against the old message too,
/// because the loop is the behaviour rather than the defect. **The assertion
/// that was RED is the last one**, which pins the advice as one a caller can act
/// on: retry only where a re-read moves the version.
///
/// The message names three causes and confirms none of them. A caller who
/// reaches the record already knows it exists, and one who does not gets
/// `NOT_FOUND` on the re-read and learns nothing it could not learn anyway — so
/// naming "may read but not edit" beside the other two discloses nothing.
#[tokio::test]
async fn the_refusal_an_unwidened_edit_reaches_advises_a_retry_that_can_succeed() {
    let c = two_projects_two_users("td_edit_advice_followable").await;
    let scope = c.scope_with(P_A, U3, &[], Some(shipped_setting()));

    let before = c
        .read_as(&scope, &c.u3_team)
        .await
        .expect("ADR-0522 grants the owner outside the team their own record");
    let version = before.meta.expect("meta").version;

    let err = c
        .edit_as(&scope, &c.u3_team, "renamed")
        .await
        .expect_err("the edit path is not widened by a read-named setting");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);

    // THE LOOP, pinned: the re-read the message sends the caller to succeeds and
    // returns the SAME version, so the retry it advises is byte-identical to the
    // request that just failed.
    let after = c
        .read_as(&scope, &c.u3_team)
        .await
        .expect("re-reading still succeeds, which is what makes the advice a loop");
    assert_eq!(
        after.meta.expect("meta").version,
        version,
        "the version has not moved, so a retry cannot differ from the attempt that failed"
    );

    // THE ASSERTION THIS TEST EXISTS FOR. The message is an interface a caller
    // writes a retry loop against, so it is asserted whole rather than by
    // substring.
    assert_eq!(
        err.message(),
        "version mismatch, no such task in this scope, or a task this caller may read but not \
         edit. Re-read: if the version has moved, retry with the new one. If the read returns \
         the same version, or returns nothing, retrying will fail identically."
    );
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
///
/// **THIS TEST STATES [`ladder_only`], AND IT IS THE ONE TEST IN THE SUITE THAT
/// HAD TO START DOING SO.** The caller here OWNS the row, so under the shipped
/// `ON` ADR-0522's arm returns it whatever the rung says — measured: rendering
/// the rung as `visibility = 1` instead of `visibility NOT IN (2, 3)` then
/// survives the whole suite. Stating `OFF` removes the arm and leaves the rung
/// as the only thing in the statement that can answer.
#[tokio::test]
async fn a_row_with_an_unrecognised_visibility_falls_back_to_private() {
    let c = two_projects_two_users("td_vis_zero").await;
    c.set_visibility(&c.u1_private, 0, "").await;
    let ladder = Some(ladder_only());

    let owner = c
        .read_as(&c.scope_with(P_A, U1, &[], ladder.clone()), &c.u1_private)
        .await
        .expect("the owner must not lose a record to a value nobody anticipated");
    assert_eq!(owner.title, "u1 private");

    let err = c
        .read_as(&c.scope_with(P_A, U2, &[], ladder), &c.u1_private)
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
