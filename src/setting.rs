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
mod tests;
