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
//! **task-db IS IN THE ENFORCING STATE, AND THIS MODULE IS WHAT PUT IT THERE.**
//! The contract admits exactly two states and names the discriminator: "WHICH
//! ONE IT IS IN IS DECIDED BY WHETHER IT READS THIS FIELD… A -db that reads it
//! is in the ENFORCING state and the refusal binds it absolutely." `Reach`
//! reads it, on `GetTask` and `ListTasks`, through [`owner_reach`]. There is no
//! third state in which this tolerates an unset setting, so a read carrying
//! none is refused rather than answered from a default.
//!
//! **THE BLOCKER THAT KEPT THIS UNWIRED IS GONE, AND IT WAS A CONTRACT PIN
//! RATHER THAN A POLICY.** `task` v0.4.1 pinned proto v1.7.1, whose `Scope` had
//! no field 6, and prost DISCARDS unknown fields rather than round-tripping
//! them — so the gateway populated a field that was erased one hop later, and
//! enforcing would have refused every read. `task` v0.4.3 vendors the contract
//! carrying `owner_reads_own_record = 6`, so the field survives the hop and
//! reaches this store intact.
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

/// Which of a caller's OWN records ownership alone reaches, once [`resolve`]
/// has answered for every team the setting can answer differently for.
///
/// **THE RESOLUTION IS PER-RECORD AND A STATEMENT IS PER-QUERY, AND THIS IS
/// WHAT BRIDGES THEM.** `Reach` builds a `WHERE` clause before any row is
/// fetched, so it cannot hand [`resolve`] the team of a row it has not read
/// yet — and reading the row first and filtering afterwards is the leak
/// `sql.rs` exists to refuse. The domain is finite, though: a setting answers
/// one value for each team it NAMES and one value for every team it does not,
/// so resolving each named team plus one team it does not name covers every row
/// a query could return.
///
/// **IT IS A PROJECTION OF [`resolve`], NEVER A SECOND COPY OF THE ALGORITHM.**
/// Every answer below comes back from [`resolve`] itself; what is added here is
/// only the enumeration of the domain. The contract's reason applies with full
/// force — "two normative copies in two files must stay in step for ever, and
/// the copy that drifts is the one a reader happens to open" — so no branch
/// here reads `org_locked`, `org_value` or the map's values.
pub struct OwnerReach {
    default_on: bool,
    exceptions: Vec<String>,
}

impl OwnerReach {
    /// What the setting answers for every team it does not name.
    pub fn default_on(&self) -> bool {
        self.default_on
    }

    /// The teams whose answer DIFFERS from [`OwnerReach::default_on`], sorted.
    ///
    /// SORTED BECAUSE `team_override` IS A HASH MAP. Its iteration order varies
    /// between processes, so an unsorted list renders a different statement on
    /// every run — which costs the engine its statement cache and makes a
    /// failing test unreproducible on the next invocation.
    pub fn exceptions(&self) -> &[String] {
        &self.exceptions
    }

    /// Whether ownership alone reaches NOTHING. That is the pre-ADR-0522
    /// behaviour, and it is the shape that renders no arm at all rather than an
    /// arm nothing can satisfy.
    pub fn reaches_nothing(&self) -> bool {
        !self.default_on && self.exceptions.is_empty()
    }
}

/// Project [`resolve`] onto every row one query could return.
///
/// **EVERY REFUSAL THE SETTING CARRIES IS REACHED EAGERLY, INCLUDING ONE FOR A
/// TEAM THE QUERY MAY NEVER TOUCH.** A team entry holding the zero refuses the
/// whole read rather than only the rows of that team. The narrower behaviour is
/// not expressible here and stating why is the point: a row whose team states
/// nothing must be REFUSED, and a `WHERE` clause can only fail to match it —
/// which turns the refusal into an invisible absence, the exact silent policy
/// this setting exists to make unwritable. Refusing wider is loud, and the
/// write path declares such an entry impossible in the first place.
pub fn owner_reach(setting: &Option<InheritedSetting>) -> Result<OwnerReach, Status> {
    let default_on =
        resolve(setting, &team_the_setting_does_not_name(setting))? == SettingValue::On;

    let mut exceptions = Vec::new();
    if let Some(present) = setting.as_ref() {
        for team in present.team_override.keys() {
            // EVERY named team is resolved, including under a lock that makes
            // the map inert. That is not wasted work: it is what keeps the
            // answer coming from `resolve` rather than from a branch here that
            // decided for itself that the map could be skipped.
            if (resolve(setting, team)? == SettingValue::On) != default_on {
                exceptions.push(team.clone());
            }
        }
    }
    exceptions.sort();

    Ok(OwnerReach {
        default_on,
        exceptions,
    })
}

/// A team id the setting does NOT name, so that resolving against it yields the
/// answer every unnamed team gets.
///
/// The empty string is the first candidate because it is not a guess: it is
/// what the `team_id` column holds for every record that is not TEAM-visible,
/// and `SetInheritedSetting` refuses an empty team id at team scope — so a
/// well-formed map never names it. A MALFORMED map CAN name it, and the answer
/// for unnamed teams would then be read off an entry that names one, so the
/// probe grows until it is genuinely absent rather than assuming a shape the
/// wire does not enforce.
fn team_the_setting_does_not_name(setting: &Option<InheritedSetting>) -> String {
    let mut probe = String::new();
    while setting
        .as_ref()
        .is_some_and(|s| s.team_override.contains_key(&probe))
    {
        probe.push('\0');
    }
    probe
}

#[cfg(test)]
mod tests {
    use super::{owner_reach, resolve};
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
}
