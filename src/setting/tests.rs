use super::{owner_reach, resolve};
use crate::pb::yadgar::common::v1::{InheritedSetting, Scope, SettingValue};
use tonic::Code;

const RECORD_TEAM: &str = "t1";
const OTHER_TEAM: &str = "t2";

/// Build a setting the way a store would: the organisation's value, its
/// lock, and zero or more team overrides.
fn org(value: SettingValue, locked: bool, overrides: &[(&str, SettingValue)]) -> InheritedSetting {
    InheritedSetting {
        org_value: value as i32,
        org_locked: locked,
        team_override: overrides
            .iter()
            .map(|(team, v)| ((*team).to_string(), *v as i32))
            .collect(),
    }
}

// -----------------------------------------------------------------------
// The truth table this car exists for: what the ORGANISATION states, by
// what the RECORD'S TEAM states. Four combinations, four named cases, and
// no row that a sibling row's answer would also satisfy.
// -----------------------------------------------------------------------

/// Row 1, in the form the wire actually produces when nothing is
/// configured: the message itself is absent.
///
/// The assertion is on the REFUSAL rather than on a resolved value, because
/// there is no value a receiver may choose here. Answering OFF would apply
/// the strict policy in a deployment that never chose it, and answering ON
/// would widen who reads a record — which the contract calls a wider
/// authority than `SetUserAdmin`.
#[test]
fn neither_level_states_anything_so_the_read_is_refused() {
    let err = resolve(&None, RECORD_TEAM).expect_err(
        "an absent setting names no policy, and a store may not pick one on its behalf",
    );
    assert_eq!(err.code(), Code::InvalidArgument);
}

/// Row 1 again, in its OTHER wire form. The contract says these are "ONE
/// CASE, NOT TWO, AND BOTH ARE REFUSED", and they are distinguishable on
/// the wire — which is exactly why both are asserted.
#[test]
fn an_organisation_row_that_states_nothing_is_refused_though_the_message_is_present() {
    let setting = Some(org(SettingValue::Unspecified, false, &[]));
    let err = resolve(&setting, RECORD_TEAM)
        .expect_err("a present message holding the zero still names no policy");
    assert_eq!(err.code(), Code::InvalidArgument);
}

/// **Row 2, and it is the row that discriminates the ORDER of the
/// branches.** The organisation states nothing; the record's team states
/// something. An implementation that tests the lock first finds it clear,
/// finds an override for this team, and answers ON — never reading
/// `org_value` at all, so its refusal is unreachable in precisely the
/// deployment that has an unconfigured organisation and a team with an
/// opinion.
///
/// The override is on `RECORD_TEAM` on purpose. Written with an empty map,
/// or with an override on some other team, this row is satisfied by the
/// wrong-order implementation too and pins nothing.
#[test]
fn a_team_override_does_not_rescue_an_organisation_that_stated_nothing() {
    let setting = Some(org(
        SettingValue::Unspecified,
        false,
        &[(RECORD_TEAM, SettingValue::On)],
    ));
    let err = resolve(&setting, RECORD_TEAM).expect_err(
        "the refusal is the FIRST step of the resolution, not a check beside it — \
         an override must not reach past an organisation that stated nothing",
    );
    assert_eq!(err.code(), Code::InvalidArgument);
}

/// Row 3: the organisation states a value, the record's team has no
/// opinion. Absence in the map is how a team states nothing, so the
/// organisation's value stands.
///
/// The override present here belongs to a DIFFERENT team, which is what
/// makes this row distinguishable from row 4: an implementation that
/// returns the first entry of the map, or ignores the key, answers ON.
#[test]
fn an_organisation_that_states_a_value_answers_for_a_team_with_no_opinion() {
    let setting = Some(org(
        SettingValue::Off,
        false,
        &[(OTHER_TEAM, SettingValue::On)],
    ));
    assert_eq!(
        resolve(&setting, RECORD_TEAM).expect("a stated organisation value resolves"),
        SettingValue::Off,
        "the override belongs to another team; this record's team states nothing"
    );
}

/// Row 4: both levels state something and the organisation is not locked,
/// so the record's team wins.
///
/// The two values CONTRADICT deliberately. An override agreeing with the
/// organisation is satisfied by an implementation that ignores the map
/// entirely.
#[test]
fn a_team_that_states_its_own_value_overrides_an_unlocked_organisation() {
    let setting = Some(org(
        SettingValue::Off,
        false,
        &[(RECORD_TEAM, SettingValue::On)],
    ));
    assert_eq!(
        resolve(&setting, RECORD_TEAM).expect("an unlocked organisation yields to its team"),
        SettingValue::On,
    );
}

// -----------------------------------------------------------------------
// The lock. A SECOND two-by-two, and the contract calls `org_locked` "THE
// LOAD-BEARING FIELD": without it an organisation cannot state a policy a
// team may not escape, which is the only reason an organisation level
// exists rather than only a team one.
// -----------------------------------------------------------------------

/// The first of the two combinations the contract names by hand. A locked
/// organisation IGNORES every override rather than merging with it, so an
/// implementation that merely prefers the override where one exists answers
/// ON here and is wrong.
#[test]
fn a_locked_organisation_ignores_a_contradicting_override() {
    let setting = Some(org(
        SettingValue::Off,
        true,
        &[(RECORD_TEAM, SettingValue::On)],
    ));
    assert_eq!(
        resolve(&setting, RECORD_TEAM).expect("a locked organisation resolves to its own value"),
        SettingValue::Off,
        "the lock is what makes an organisation's policy inescapable"
    );
}

/// The lock changes nothing when no team disagrees. Asserted so that the
/// row above cannot be satisfied by an implementation that has simply
/// stopped reading the map.
#[test]
fn a_locked_organisation_with_no_override_answers_its_own_value() {
    let setting = Some(org(SettingValue::On, true, &[]));
    assert_eq!(
        resolve(&setting, RECORD_TEAM).expect("a locked organisation resolves"),
        SettingValue::On,
    );
}

// -----------------------------------------------------------------------
// The two properties the truth table alone does not reach.
// -----------------------------------------------------------------------

/// **The team branch validates its answer too, and is not exempt because a
/// zero-holding entry is declared impossible.** The map is
/// `map<string, SettingValue>`, so a zero-holding entry is REPRESENTABLE
/// and round-trips intact — a migration that materialises a row per team, a
/// backfill from a join, or any writer predating the write path's refusal
/// produces one. Without this branch the store resolves UNSPECIFIED and
/// applies the strict policy silently, which is the organisation branch's
/// defect moved one line down.
#[test]
fn a_team_entry_holding_the_unspecified_zero_is_refused_rather_than_applied() {
    let setting = Some(org(
        SettingValue::On,
        false,
        &[(RECORD_TEAM, SettingValue::Unspecified)],
    ));
    let err = resolve(&setting, RECORD_TEAM)
        .expect_err("a zero-holding override states nothing and cannot be applied");
    assert_eq!(err.code(), Code::InvalidArgument);
}

/// A value no member of the enum names is refused rather than guessed.
/// proto3 enums are OPEN, so an unrecognised number arrives intact instead
/// of collapsing to the zero; a `match` with a permissive fallthrough would
/// answer a policy that does not exist.
#[test]
fn an_unrecognised_organisation_value_is_refused_rather_than_guessed() {
    let setting = Some(InheritedSetting {
        org_value: 99,
        org_locked: true,
        team_override: Default::default(),
    });
    let err =
        resolve(&setting, RECORD_TEAM).expect_err("99 names no policy this contract declares");
    assert_eq!(err.code(), Code::InvalidArgument);
}

// -----------------------------------------------------------------------
// The PROJECTION. `resolve` answers for ONE record's team; a statement is
// built before any record is read. These pin the bridge between the two,
// and every one of them is a claim about what SQL will be rendered.
// -----------------------------------------------------------------------

/// The refusal survives the projection, and it is the case that turns this
/// service from PRE-ENFORCEMENT into ENFORCING. An absent setting is what
/// the wire carried until `task` v0.4.3, so this is the assertion that
/// makes the roll-out order load-bearing rather than advisory.
#[test]
fn an_absent_setting_refuses_the_whole_read_rather_than_reaching_nothing() {
    let err = owner_reach(&None)
        .err()
        .expect("an absent setting names no policy, and a read may not proceed on one");
    assert_eq!(err.code(), Code::InvalidArgument);
}

/// The other wire form of the same case. Refusing only one of the two is
/// how a wholly absent setting gets accepted somewhere else.
#[test]
fn a_present_setting_stating_nothing_refuses_the_whole_read_too() {
    let setting = Some(org(SettingValue::Unspecified, false, &[]));
    let err = owner_reach(&setting)
        .err()
        .expect("a present message holding the zero still names no policy");
    assert_eq!(err.code(), Code::InvalidArgument);
}

/// **The precedence, seen through the projection.** The organisation states
/// nothing and the record's own team states ON. An implementation that
/// enumerates the map before resolving the organisation answers "this team
/// reaches" and never refuses.
#[test]
fn a_team_override_does_not_rescue_the_projection_of_a_silent_organisation() {
    let setting = Some(org(
        SettingValue::Unspecified,
        false,
        &[(RECORD_TEAM, SettingValue::On)],
    ));
    let err = owner_reach(&setting)
        .err()
        .expect("the refusal is the first step of the resolution, and the projection keeps it");
    assert_eq!(err.code(), Code::InvalidArgument);
}

/// A team entry holding the representable zero refuses the read, EVEN
/// THOUGH no row of that team need be returned. Stated as its own test
/// because the eager refusal is a deliberate widening, not a side effect.
#[test]
fn a_zero_holding_team_entry_refuses_the_read_though_the_query_may_not_touch_that_team() {
    let setting = Some(org(
        SettingValue::On,
        false,
        &[(OTHER_TEAM, SettingValue::Unspecified)],
    ));
    let err = owner_reach(&setting)
        .err()
        .expect("a zero-holding override states nothing and cannot be projected");
    assert_eq!(err.code(), Code::InvalidArgument);
}

/// **THE SHIPPED CONFIGURATION.** `iam-db` migration 12 inserts
/// `owner_reads_own_record` as `SETTING_VALUE_ON` with the lock ENGAGED, so
/// this is the shape every deployment gets: ownership reaches every record
/// the caller owns, and no team is an exception.
#[test]
fn a_locked_organisation_stating_on_reaches_every_record_the_caller_owns() {
    let setting = Some(org(SettingValue::On, true, &[]));
    let reach = owner_reach(&setting).expect("a locked organisation resolves");

    assert!(reach.default_on());
    assert!(reach.exceptions().is_empty());
    assert!(!reach.reaches_nothing());
}

/// The lock makes the map INERT, so a contradicting override is not an
/// exception — it is nothing at all. An implementation that enumerated the
/// map without resolving each key reports `t1` here and renders an arm that
/// excludes a team the organisation locked in.
#[test]
fn a_locked_organisation_projects_no_exception_for_a_contradicting_override() {
    let setting = Some(org(
        SettingValue::On,
        true,
        &[(RECORD_TEAM, SettingValue::Off)],
    ));
    let reach = owner_reach(&setting).expect("a locked organisation resolves");

    assert!(reach.default_on());
    assert!(
        reach.exceptions().is_empty(),
        "the lock makes the map inert; {:?} is not an exception to anything",
        reach.exceptions()
    );
}

/// OFF everywhere is the PRE-ENFORCEMENT reach expressed as a policy
/// somebody chose, and it must render no arm at all rather than an arm no
/// row satisfies.
#[test]
fn a_locked_organisation_stating_off_reaches_nothing() {
    let setting = Some(org(SettingValue::Off, true, &[]));
    let reach = owner_reach(&setting).expect("a locked organisation resolves");

    assert!(!reach.default_on());
    assert!(reach.reaches_nothing());
}

/// An unlocked OFF organisation with one team saying ON. The default is
/// OFF, so the arm names the teams that DO reach — and `t1` is that list.
#[test]
fn an_unlocked_organisation_stating_off_names_the_teams_that_do_reach() {
    let setting = Some(org(
        SettingValue::Off,
        false,
        &[(RECORD_TEAM, SettingValue::On)],
    ));
    let reach = owner_reach(&setting).expect("an unlocked organisation yields to its team");

    assert!(!reach.default_on());
    assert_eq!(reach.exceptions(), [RECORD_TEAM.to_string()]);
    assert!(
        !reach.reaches_nothing(),
        "one team states ON, so ownership reaches something"
    );
}

/// The mirror, and it is a DIFFERENT rendering rather than the same one
/// inverted: the default is ON, so the arm names the teams that do NOT
/// reach. A projection that only ever emitted an inclusion list would
/// answer "only t1 reaches" here, which is the complement of the truth.
#[test]
fn an_unlocked_organisation_stating_on_names_the_teams_that_do_not_reach() {
    let setting = Some(org(
        SettingValue::On,
        false,
        &[(RECORD_TEAM, SettingValue::Off)],
    ));
    let reach = owner_reach(&setting).expect("an unlocked organisation yields to its team");

    assert!(reach.default_on());
    assert_eq!(reach.exceptions(), [RECORD_TEAM.to_string()]);
}

/// A team that AGREES with the organisation is not an exception. Listing it
/// would render `team_id NOT IN ('t1')` for a team whose answer is ON,
/// which excludes exactly the rows the setting includes.
#[test]
fn a_team_agreeing_with_its_organisation_is_not_an_exception() {
    let setting = Some(org(
        SettingValue::On,
        false,
        &[
            (RECORD_TEAM, SettingValue::On),
            (OTHER_TEAM, SettingValue::Off),
        ],
    ));
    let reach = owner_reach(&setting).expect("an unlocked organisation resolves");

    assert!(reach.default_on());
    assert_eq!(
        reach.exceptions(),
        [OTHER_TEAM.to_string()],
        "only the team that DISAGREES is an exception"
    );
}

/// **THE EMPTY TEAM ID IS A REAL KEY, NOT A SENTINEL, AND THIS IS WHY THE
/// PROBE GROWS.** `team_id` is `NOT NULL DEFAULT ''`, so every record that
/// is not TEAM-visible carries the empty string — and a projection that
/// probed the unnamed answer at `""` would read it off THIS entry and
/// report OFF as the answer for every team in the deployment.
///
/// The organisation states ON and only the empty team disagrees, so the
/// correct projection is default ON with `""` as its single exception.
#[test]
fn an_override_on_the_empty_team_does_not_become_the_answer_for_every_other_team() {
    let setting = Some(org(SettingValue::On, false, &[("", SettingValue::Off)]));
    let reach = owner_reach(&setting).expect("an unlocked organisation resolves");

    assert!(
        reach.default_on(),
        "the empty team's entry must not be read as the answer for teams the setting does not name"
    );
    assert_eq!(reach.exceptions(), [String::new()]);
}

/// The rendered statement must not depend on hash iteration order.
#[test]
fn the_exceptions_are_sorted_so_one_setting_renders_one_statement() {
    let setting = Some(org(
        SettingValue::Off,
        false,
        &[
            ("t-zulu", SettingValue::On),
            ("t-alpha", SettingValue::On),
            ("t-mike", SettingValue::On),
        ],
    ));
    let reach = owner_reach(&setting).expect("an unlocked organisation resolves");

    assert_eq!(
        reach.exceptions(),
        [
            "t-alpha".to_string(),
            "t-mike".to_string(),
            "t-zulu".to_string()
        ]
    );
}

/// **The team is the RECORD'S, never the CALLER'S.** The defect ADR-0522
/// answers is an owner who LEFT the team their record is shared with, so a
/// resolution keyed on the caller's current membership evaporates in
/// exactly the case it is for.
///
/// The caller here belongs to NO team at all, and the record's team still
/// resolves its own override. The `Scope` is built and deliberately not
/// passed: it is the argument this function must never take.
#[test]
fn the_override_is_keyed_on_the_records_team_not_the_callers() {
    let caller = Scope {
        user_id: "u1".to_string(),
        project_id: "acme".to_string(),
        team_ids: vec![],
        ..Default::default()
    };
    assert!(
        caller.team_ids.is_empty(),
        "the caller must belong to no team for this test to say anything"
    );
    let setting = Some(org(
        SettingValue::Off,
        false,
        &[(RECORD_TEAM, SettingValue::On)],
    ));
    assert_eq!(
        resolve(&setting, RECORD_TEAM).expect("the record's own team states a value"),
        SettingValue::On,
        "an owner who left the team must still get their record's policy"
    );
}
