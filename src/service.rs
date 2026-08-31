//! `TaskDbService`: the RPC surface and its instrumentation.
//!
//! One RPC is one transaction (D5), and scope is enforced on every path rather
//! than trusted from the caller. The operations themselves live next door —
//! `read` for the two that answer questions, `write` for the three that change
//! something — because what a handler does HERE is start a `Call`, hand off, and
//! record what came back. Keeping the boundary and the statements in one file is
//! what made a missing `WHERE` clause hard to see.

use sqlx::MySqlPool;
use tonic::{Request, Response, Status};
use yadgar_telemetry::estimator::Class;
use yadgar_telemetry::grpc::status_name;
use yadgar_telemetry::observe::{Call, Outcome};
use yadgar_telemetry::pb::yadgar::telemetry::v1::Kind;

use crate::pb::yadgar::task::v1::task_db_service_server::TaskDbService;
use crate::pb::yadgar::task::v1::*;
use crate::sql::tel_scope;

pub struct TaskDb {
    pub(crate) pool: MySqlPool,
}

impl TaskDb {
    pub fn new(pool: MySqlPool) -> Self {
        Self { pool }
    }
}

/// What every handler records. `rows` is the one field that differs, and it is
/// the one a blanket value gets wrong: the row count for a list is the LIST, not
/// one, or every page looks like a single-row read.
fn envelope<T: prost::Message + std::fmt::Debug>(response: &T, rows: u32) -> Outcome {
    Outcome {
        status: "OK",
        payload: format!("{response:?}"),
        encoded_bytes: Some(prost::Message::encoded_len(response) as u64),
        class: Class::Envelope,
        rows,
        ..Default::default()
    }
}

#[tonic::async_trait]
impl TaskDbService for TaskDb {
    async fn create_task(
        &self,
        request: Request<CreateTaskRequest>,
    ) -> Result<Response<CreateTaskResponse>, Status> {
        let req = request.into_inner();
        let call = Call::start("task-db", "CreateTask", Kind::Write, tel_scope(&req.scope));

        call.run(
            async move { self.create(req).await },
            |r| envelope(r, 1),
            status_name,
        )
        .await
        .map(Response::new)
    }

    async fn get_task(
        &self,
        request: Request<GetTaskRequest>,
    ) -> Result<Response<GetTaskResponse>, Status> {
        let req = request.into_inner();
        let call = Call::start("task-db", "GetTask", Kind::Read, tel_scope(&req.scope));

        call.run(
            async move { self.get(req).await },
            |r| envelope(r, 1),
            status_name,
        )
        .await
        .map(Response::new)
    }

    async fn list_tasks(
        &self,
        request: Request<ListTasksRequest>,
    ) -> Result<Response<ListTasksResponse>, Status> {
        let req = request.into_inner();
        let call = Call::start("task-db", "ListTasks", Kind::Read, tel_scope(&req.scope));

        call.run(
            async move { self.list(req).await },
            |r| envelope(r, r.tasks.len() as u32),
            status_name,
        )
        .await
        .map(Response::new)
    }

    async fn update_task(
        &self,
        request: Request<UpdateTaskRequest>,
    ) -> Result<Response<UpdateTaskResponse>, Status> {
        let req = request.into_inner();
        let call = Call::start("task-db", "UpdateTask", Kind::Write, tel_scope(&req.scope));

        call.run(
            async move { self.update(req).await },
            |r| envelope(r, 1),
            status_name,
        )
        .await
        .map(Response::new)
    }

    async fn delete_task(
        &self,
        request: Request<DeleteTaskRequest>,
    ) -> Result<Response<DeleteTaskResponse>, Status> {
        let req = request.into_inner();
        let call = Call::start("task-db", "DeleteTask", Kind::Write, tel_scope(&req.scope));

        // DeleteTaskResponse is empty — nothing to measure. Recorded anyway: a
        // delete costs time and belongs in the count, and omitting it would make
        // deletes invisible.
        call.run(
            async move { self.delete(req).await },
            |r| envelope(r, 1),
            status_name,
        )
        .await
        .map(Response::new)
    }
}
