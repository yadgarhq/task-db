//! ADR-0522's inheritable setting, resolved against the team of the ROW.
//!
//! **THIS MODULE IS THE RESOLUTION AND NOTHING ELSE.** `yadgar/common/v1`
//! states the algorithm once, normatively, and this is that algorithm written
//! in Rust — "RESOLVED WHERE THE REACH IS COMPUTED, NEVER AT A CALLER", and
//! this service is where the reach is computed. `iam` stores the setting, the
//! gateway attests it onto `Scope`, and neither of them can answer it: the
//! answer depends on the team of the record being read, which nothing upstream
//! of the query knows.
//!
//! **task-db IS STILL IN THE PRE-ENFORCEMENT STATE, DELIBERATELY, AND THIS
//! FUNCTION IS NOT CALLED FROM THE REQUEST PATH.** The contract admits exactly
//! two states and names the discriminator: "WHICH ONE IT IS IN IS DECIDED BY
//! WHETHER IT READS THIS FIELD… A -db that does not read the field is in the
//! PRE-ENFORCEMENT state and behaves exactly as it does today — that is what
//! makes the order above possible at all." Enforcing today would refuse every
//! read, because the hop between the gateway and this store cannot carry the
//! field: `task` v0.4.1 pins proto v1.7.1, whose `Scope` has no field 6, and
//! prost DISCARDS unknown fields rather than round-tripping them. So the
//! gateway populates a field that is dropped one hop later, and a `Reach` built
//! on this resolution would see an absent setting on every call and refuse it —
//! correctly, and catastrophically. Wiring it in is the step AFTER `task`
//! advances its own pin, and it is a change to `sql.rs`, not to this file.
//!
//! **THE PRESENCE CHECK IS RULED BEHAVIOUR THAT NO TEST CAN PIN, AND THAT IS
//! RECORDED HERE SO NOBODY LATER "SIMPLIFIES" IT AWAY AS DEAD.** Substituting
//! `unwrap_or_default()` for the `as_ref()` refusal below is an EQUIVALENT
//! mutant: `InheritedSetting::default()` holds `org_value` UNSPECIFIED, so the
//! defaulted message lands on the very next line's refusal anyway, and every
//! present input is returned unchanged. The two spellings agree on every
//! input — the contract says so itself, calling the accident by name: "Reading
//! org_value off an absent message yields the zero in Go and in the C++ family
//! and lands on the refusal by accident." The check is written explicitly
//! because a resolution that is correct only where the language's zero happens
//! to agree is correct by luck, and because the next field added to this
//! message would end the coincidence silently.
//!
//! **THE REFUSAL IS THE FIRST STEP, NOT A CHECK BESIDE IT.** An implementation
//! that branches on the lock first reaches an answer without ever inspecting
//! `org_value` — with the lock clear and an override present for that team, the
//! override wins and the organisation is never consulted. The refusal is then
//! unreachable in precisely the deployment that needs it, and the store answers
//! a policy nobody stated. `org_locked` is a bool and cannot say "unknown";
//! `org_value` is the only field here that can, so it is the only one that can
//! carry the refusal, and it carries nothing unless it is read first.

use tonic::Status;

use crate::pb::yadgar::common::v1::{InheritedSetting, SettingValue};

/// Resolve one inheritable setting for one record.
///
/// `record_team` is the `team_id` of the ROW being read, NEVER the teams the
/// caller belongs to. The failure ADR-0522 exists to fix is an owner who LEFT
/// the team their record is shared with, so keying the override on the caller's
/// current membership would make the setting evaporate in exactly the case it
/// is for. The signature is what enforces that: a `Scope` is not an argument.
///
/// An absent message and a present one holding `SETTING_VALUE_UNSPECIFIED` are
/// ONE case and both are refused. Presence is not a third value — it is
/// checked first because a language with explicit message presence must unwrap
/// before it can read the field at all, and a verbatim implementation would
/// otherwise have no ruled behaviour at that step.
pub fn resolve(
    setting: &Option<InheritedSetting>,
    record_team: &str,
) -> Result<SettingValue, Status> {
    // STEP ONE, AND IT TESTS THE MESSAGE RATHER THAN ONLY THE FIELD. `stated`
    // folds the absent message and the present-but-unspecified one into the one
    // case the contract says they are, so neither can be answered by accident.
    let setting = setting
        .as_ref()
        .ok_or_else(|| refusal("the organisation states no value"))?;
    let org_value = stated(setting.org_value, "the organisation")?;

    // A locked organisation IGNORES every override rather than merging with it.
    // This is the field that lets an organisation state a policy a team may not
    // escape, and it is read AFTER org_value so the refusal above stays
    // reachable in a deployment whose organisation row is missing.
    if setting.org_locked {
        return Ok(org_value);
    }

    // Absence is how a team states nothing, so a team with no opinion has no
    // entry — and the organisation's value stands.
    match setting.team_override.get(record_team) {
        // BOTH BRANCHES THAT PRODUCE A VALUE VALIDATE IT. A zero-holding entry
        // is declared impossible by the write path and is still representable
        // on the wire, and a resolution that trusts its input is a resolution
        // whose correctness lives somewhere else.
        Some(&entry) => stated(entry, &format!("team {record_team}")),
        None => Ok(org_value),
    }
}

/// The one enum value that is not an answer, refused in both branches.
///
/// An unrecognised NUMBER lands here too, and deliberately: proto3 enums are
/// open, so a value no member names arrives intact rather than collapsing to
/// the zero. Answering it would apply a policy this contract does not declare.
fn stated(value: i32, who: &str) -> Result<SettingValue, Status> {
    match SettingValue::try_from(value) {
        Ok(SettingValue::Off) => Ok(SettingValue::Off),
        Ok(SettingValue::On) => Ok(SettingValue::On),
        _ => Err(refusal(&format!("{who} states no value"))),
    }
}

/// Every refusal in this module, worded the same way and carrying the same
/// code. `INVALID_ARGUMENT` because the request named no policy — nothing here
/// failed, and nothing is retryable.
fn refusal(what: &str) -> Status {
    Status::invalid_argument(format!(
        "{what} for owner_reads_own_record; a store may not choose one on its behalf"
    ))
}

#[cfg(test)]
mod tests {
    use super::resolve;
    use crate::pb::yadgar::common::v1::{InheritedSetting, Scope, SettingValue};
    use tonic::Code;

    const RECORD_TEAM: &str = "t1";
    const OTHER_TEAM: &str = "t2";

    /// Build a setting the way a store would: the organisation's value, its
    /// lock, and zero or more team overrides.
    fn org(
        value: SettingValue,
        locked: bool,
        overrides: &[(&str, SettingValue)],
    ) -> InheritedSetting {
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
            resolve(&setting, RECORD_TEAM)
                .expect("a locked organisation resolves to its own value"),
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
}
