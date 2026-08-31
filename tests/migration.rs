//! Migration 5, `heal_unspecified_visibility`, and the state it exists for.
//!
//! **These tests were impossible until the fixture could stop short.** The rows
//! migration 5 heals are rows the CURRENT code cannot write, so a database built
//! from the whole migration set never contains one and the migration was a
//! no-op against every fixture in the suite — untested not for want of a test
//! but for want of a seam. `World::fresh_at` is that seam: a database at 4, the
//! rows the old writer left, and then the ordinary `apply` over the top.
//!
//! Both halves matter. The column is what the migration changes; the READS are
//! what the change was for, and a heal the query still fails to match would be
//! the same loss of access with a tidier table.

mod support;

use tonic::Code;

use support::{World, P_A, TEAM, U1, U2, U3};
use yadgar_task_db::pb::yadgar::common::v1::Visibility;

/// What `unwrap_or(1)` accepted from a caller that set nothing: the proto says
/// VISIBILITY_UNSPECIFIED is never persisted, and it was.
const UNSPECIFIED: i8 = 0;

/// A value no version of this module ever wrote, seeded anyway. Migration 5 is
/// `NOT IN (1, 2, 3)` rather than `= 0`, and the difference is whether the heal
/// covers what nobody anticipated or only the one case somebody did.
const NONSENSE: i8 = 7;

/// Ids, in the order the store returned them.
fn ids(tasks: &[yadgar_task_db::pb::yadgar::task::v1::Task]) -> Vec<String> {
    tasks
        .iter()
        .filter_map(|t| t.meta.as_ref().map(|m| m.id.clone()))
        .collect()
}

/// The column itself.
///
/// The two control rows are not decoration. `UPDATE task SET visibility = 1`
/// with its `WHERE` dropped or loosened would collapse every TEAM and ORG
/// record to PRIVATE, and a test that seeded only the broken rows would pass
/// while it happened — the failure would surface as records quietly
/// disappearing from the people they were shared with.
#[tokio::test]
async fn migration_5_heals_only_the_values_the_enum_forbids() {
    let w = World::fresh_at("td_mig5_column", 4).await;

    let unspecified = w.seed_row(P_A, U1, 1, UNSPECIFIED, "").await;
    let nonsense = w.seed_row(P_A, U1, 2, NONSENSE, "").await;
    let team = w.seed_row(P_A, U3, 3, Visibility::Team as i8, TEAM).await;
    let org = w.seed_row(P_A, U3, 4, Visibility::Org as i8, "").await;

    let applied = w.migrate_to_head().await;

    let private = Visibility::Private as i8;
    assert_eq!(
        w.stored_visibility(&unspecified).await,
        private,
        "a row carrying VISIBILITY_UNSPECIFIED must be healed to PRIVATE"
    );
    assert_eq!(
        w.stored_visibility(&nonsense).await,
        private,
        "the heal is NOT IN (1, 2, 3), so a value outside the enum lands on \
         PRIVATE too — the most restrictive rung that still has an owner"
    );
    assert_eq!(
        w.stored_visibility(&team).await,
        Visibility::Team as i8,
        "a TEAM row is already valid and must survive untouched: healing it \
         would take a shared record away from the team it was shared with"
    );
    assert_eq!(
        w.stored_visibility(&org).await,
        Visibility::Org as i8,
        "an ORG row is already valid and must survive untouched"
    );

    // Last on purpose. It corroborates that migration 5 is what did the above
    // and asserts nothing about the rows itself, so putting it first would let
    // a bookkeeping detail be the failure a broken heal reports.
    assert_eq!(
        applied, 5,
        "the fixture stopped at 4, so `apply` had exactly migration 5 to run"
    );
}

/// A second `apply` runs nothing and moves nothing.
///
/// Named for what it PROVES rather than for what it is tempting to claim. This
/// is the ledger declining to re-run a recorded migration — the migration's own
/// SQL never executes a second time — so it is not evidence that migration 5 is
/// idempotent in itself. Executing the statement twice by hand would be the
/// second apply path this file exists to avoid, so the honest thing is the
/// narrower name: after migrating to head, migrating again is a no-op, and the
/// row healed on the first pass is still what it was.
#[tokio::test]
async fn a_second_apply_finds_nothing_pending() {
    let w = World::fresh_at("td_mig5_again", 4).await;
    let healed = w.seed_row(P_A, U1, 1, UNSPECIFIED, "").await;

    w.migrate_to_head().await;
    assert_eq!(
        w.migrate_to_head().await,
        5,
        "a second apply finds nothing pending and stays at 5"
    );
    assert_eq!(
        w.stored_visibility(&healed).await,
        Visibility::Private as i8,
        "and leaves the row the first pass healed alone"
    );
}

/// The half that matters: PRIVATE in the column AND private through the
/// service.
///
/// The healed row is reachable by its owner and by nobody else, on both read
/// paths — `GetTask` and `ListTasks` build the ladder separately, and a
/// predicate that agrees with the healed column on one statement and not the
/// other is the same lost access with a smaller blast radius.
///
/// The ORG and TEAM rows are read by NON-owners on purpose. Without them a
/// migration that healed every row to PRIVATE would satisfy every assertion
/// about the broken row and quietly cut two working sharing paths.
///
/// Stated plainly, because it would otherwise be discovered as a surprise: this
/// test also passes with migration 5 DELETED. `Reach::visible` renders the
/// PRIVATE rung as "not one of the wider two", so an unhealed 0 already reads
/// as private — deliberately, and that is the fail-closed compensation the
/// migration is meant to make unnecessary rather than duplicate. What this test
/// catches is a migration that heals to something the ladder does not agree
/// with, in either direction; the column assertions above are what catch its
/// absence.
#[tokio::test]
async fn a_healed_row_reads_as_private_through_the_service() {
    let w = World::fresh_at("td_mig5_reads", 4).await;

    let healed = w.seed_row(P_A, U1, 1, UNSPECIFIED, "").await;
    let team = w.seed_row(P_A, U3, 2, Visibility::Team as i8, TEAM).await;
    let org = w.seed_row(P_A, U3, 3, Visibility::Org as i8, "").await;

    w.migrate_to_head().await;

    w.read(P_A, U1, &healed)
        .await
        .expect("the owner must be able to read the row that was healed for them");

    let denied = w
        .read(P_A, U2, &healed)
        .await
        .expect_err("a PRIVATE row must not be readable by anyone but its owner");
    assert_eq!(
        denied.code(),
        Code::NotFound,
        "a row outside the caller's reach is absent, not refused: \
         PERMISSION_DENIED would confirm it exists"
    );

    // The scopes here carry no teams, so the TEAM arm cannot be what makes a
    // read pass — it is dropped from the predicate entirely when the list is
    // empty.
    let mine = ids(&w.list(P_A, U1).await);
    assert!(
        mine.contains(&healed),
        "the owner's list must carry the healed row: GetTask and ListTasks \
         build the ladder separately"
    );

    let theirs = ids(&w.list(P_A, U2).await);
    assert!(
        !theirs.contains(&healed),
        "a non-owner's list must not carry a healed PRIVATE row"
    );
    assert!(
        theirs.contains(&org),
        "the migration must not have swept the ORG row into PRIVATE: a \
         non-owner still reads it"
    );

    let teammate = w.scope_in(P_A, U2, &[TEAM]);
    w.read_as(&teammate, &team)
        .await
        .expect("a TEAM row must still reach its team after the migration runs");
}
