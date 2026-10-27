//! `LookupWrite` (ADR-1051): a read of the D9 claim ledger by idempotency key,
//! without writing.
//!
//! **WHY IT EXISTS.** The logic service reads a task before it writes it, so a
//! same-key retry whose first attempt was applied can fail that read — the task
//! was removed, or left the caller's reach — or fail a check made on what the
//! read returned, before this store ever sees the key. This RPC lets it ask the
//! ledger FIRST, so the retry is replayed (D9) rather than refused.
//!
//! **A READ.** No transaction and no lock: one autocommit `SELECT` on the
//! primary key, which sees the latest committed claim. A claim still inside an
//! uncommitted write is invisible, and that is a miss rather than a defect: the
//! claim stays authoritative, and the write that follows a miss is decided at
//! its own claim by [`super::decide`] (ADR-1051 rule 8).
//!
//! **FOUND BY EQUALITY ON `(project_id, user_id, idem_key)` AND NOTHING ELSE.**
//! Neither the task's reach nor `scope.team_ids` takes part: the claim is the
//! caller's own record, and the task it names may since have been removed or
//! moved out of reach — the case this RPC exists for. A miss reveals only that
//! this caller never claimed this key in this project.
//!
//! **A MISS IS OK WITH THE ONEOF UNSET**, never an error status: the common
//! path is a key never used, and it must stay out of error telemetry.

use tonic::Status;
use yadgar_dial::Deadline;

use super::{
    caller_fingerprint, decide, present, Presented, Recorded, CREATE_TASK, DELETE_TASK, RECORDED,
    UPDATE_TASK,
};
use crate::pb::yadgar::task::v1::lookup_write_response::Outcome;
use crate::pb::yadgar::task::v1::{LookupWriteRequest, LookupWriteResponse, WriteRpc};
use crate::service::TaskDb;
use crate::sql::{internal, scope_of};
use crate::txn;

impl TaskDb {
    pub(crate) async fn lookup(
        &self,
        req: LookupWriteRequest,
        deadline: Deadline,
    ) -> Result<LookupWriteResponse, Status> {
        // EVERY REFUSAL BEFORE ANY DATABASE ACCESS. Each is a malformed
        // request, which no state of the ledger could make well-formed.
        let scope = scope_of(&req.scope)?;
        // A write with no key records no claim, so a lookup of one could only
        // miss — and a caller that sends one has a bug a silent miss would hide.
        let key = present(req.idempotency.as_ref())
            .ok_or_else(|| Status::invalid_argument("idempotency.key is required on a lookup"))?;
        let rpc = match WriteRpc::try_from(req.rpc) {
            Ok(rpc @ (WriteRpc::CreateTask | WriteRpc::UpdateTask | WriteRpc::DeleteTask)) => rpc,
            Ok(WriteRpc::Unspecified) | Err(_) => {
                return Err(Status::invalid_argument(format!(
                    "rpc {} is not a WriteRpc this store records",
                    req.rpc
                )))
            }
        };
        // REQUIRED here, where an update or delete takes it as optional: a
        // lookup that cannot be compared would bypass D9's refusal of a
        // different request (ADR-1051 alternative D).
        let caller = caller_fingerprint(&req.caller_fingerprint)?.ok_or_else(|| {
            Status::invalid_argument("caller_fingerprint is required on a lookup, exactly 32 bytes")
        })?;

        let sql = format!(
            "SELECT {RECORDED} FROM task_write
              WHERE project_id = ? AND user_id = ? AND idem_key = ?"
        );
        let mut held = txn::acquire(&self.pool, deadline).await?;
        // AUDIT: the interpolation is this module's own column list; every
        // caller value is a bound parameter.
        let found: Option<Recorded> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(&scope.project_id)
            .bind(&scope.user_id)
            .bind(key)
            .fetch_optional(held.conn())
            .await
            .map_err(internal)?;
        let Some(row) = found else {
            return Ok(LookupWriteResponse { outcome: None });
        };

        // No store digest: this request cannot compute one, so a claim that
        // stores no caller fingerprint replays without a comparison.
        let presented = |rpc| Presented {
            rpc,
            request: None,
            caller: Some(&caller),
        };
        let outcome = match rpc {
            WriteRpc::CreateTask => Outcome::Create(decide(row, presented(CREATE_TASK))?),
            WriteRpc::UpdateTask => Outcome::Update(decide(row, presented(UPDATE_TASK))?),
            WriteRpc::DeleteTask => Outcome::Delete(decide(row, presented(DELETE_TASK))?),
            WriteRpc::Unspecified => unreachable!("refused above, before the read"),
        };
        Ok(LookupWriteResponse {
            outcome: Some(outcome),
        })
    }
}
