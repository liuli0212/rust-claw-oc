mod bus;
mod model;
mod query;
mod sqlite;

use std::sync::Arc;

use once_cell::sync::Lazy;

pub use bus::{make_attrs, trace_ctx_from_seed, unix_ms_now, TraceBus, TraceSpanHandle};
pub use model::{
    RunContextOverview, RunOverview, RunSummary, TraceActor, TraceArtifacts, TraceContext,
    TraceHotspot, TraceKind, TraceLevel, TraceRecord, TraceSeed, TraceSpanSummary, TraceStatus,
    TraceTreeNode,
};
pub use query::{
    find_run_for_subsession, get_artifacts, get_records, get_run, get_run_overview, get_tree,
    list_runs, RecordQuery, RunQuery,
};

static TRACE_BUS: Lazy<Arc<TraceBus>> = Lazy::new(|| Arc::new(TraceBus::new()));

pub fn shared_bus() -> Arc<TraceBus> {
    TRACE_BUS.clone()
}
