//! Points in the write path where a test can stop or hold a batch, and the receive clock.
//!
//! The service always runs with [`NoHooks`]: nothing in its configuration or environment selects
//! another implementation, so these exist only for the tests that start the pipeline in their
//! own process (tests/telemetry.rs). A test uses them to end the service between two steps (it
//! holds the batch at a checkpoint and aborts the pipeline), to hold a batch while another
//! replica receives it, and to fix the receive time so two attempts write identical blocks.

use std::future::Future;
use std::pin::Pin;

use protocol::{GatewayId, StreamId};

/// A step of the write path, reached in this order (rule I1, services/ingest/README.md,
/// "寫入流程").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Point {
    /// The stream's lock is held and the batch is above the high-water mark; nothing is written
    /// yet.
    Locked,
    /// The rows an earlier attempt left are deleted; the batch is not inserted yet.
    Deleted,
    /// The batch's rows are inserted (the aggregate tables' views ran in the same INSERT); the
    /// reconcile has not run and there is no completion record.
    RowsWritten,
    /// The reconcile ran; the completion record is not written yet.
    BeforeCommitRecord,
}

/// Where a batch is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    pub point: Point,
    pub gateway: GatewayId,
    pub stream: StreamId,
    pub seq: u64,
}

/// A boxed future of a hook.
pub type HookFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// What a test may do inside the write path. Every method defaults to doing nothing.
pub trait Hooks: Send + Sync + 'static {
    /// Called when a batch reaches `checkpoint`; the batch continues when the future ends.
    fn reached(&self, checkpoint: Checkpoint) -> HookFuture<'_> {
        drop(checkpoint);
        Box::pin(async {})
    }

    /// The receive time to stamp on a batch, in milliseconds since the Unix epoch, instead of
    /// the service's clock.
    fn received_at(&self) -> Option<u64> {
        None
    }
}

/// The service's hooks: none.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoHooks;

impl Hooks for NoHooks {}
