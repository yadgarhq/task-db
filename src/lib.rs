//! `task-db` — the only writer of the task store (D4).
//!
//! It holds no business rules. Its job is the boundary: one call is one
//! transaction (D5), identity and scope are enforced here, and the logic service
//! reaches this store only over gRPC — never by opening a connection of its own.

#![forbid(unsafe_code)]

pub mod boot;
mod idem;
mod read;
mod rows;
pub mod schema;
pub mod service;
pub mod setting;
mod sql;
mod write;

/// Generated from the vendored contract (D16, D70).
///
/// The module tree MIRRORS the protobuf package path, and has to: generated
/// cross-package references are emitted as `super::super::common::v1::Meta`, so
/// a flattened tree fails to compile with an error that points at generated code
/// rather than at this file.
pub mod pb {
    pub mod yadgar {
        pub mod common {
            pub mod v1 {
                tonic::include_proto!("yadgar.common.v1");
            }
        }
        pub mod task {
            pub mod v1 {
                tonic::include_proto!("yadgar.task.v1");
            }
        }
    }
}
