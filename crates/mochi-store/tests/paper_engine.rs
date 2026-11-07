//! Engine properties the scenario fixture cannot show.

use mochi_core::paper::PaperExecutionEngine;
use mochi_store::SqlitePaperStore;

// The runtime holds the engine across async tasks, so this must keep compiling.
#[test]
fn engine_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<PaperExecutionEngine<SqlitePaperStore>>();
}
