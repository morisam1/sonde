//! The flat-memory check lives in its own binary so no other test's
//! allocations disturb the measurement.

mod common;

use std::time::Duration;

use common::{postgres, resident_bytes};
use futures::{StreamExt as _, executor::block_on};

const ROWS: usize = 1_000_000;
/// The full result is over 150 MB of row data; holding a few pages is not.
const BUDGET_BYTES: usize = 48 * 1024 * 1024;

#[test]
fn stream_should_keep_memory_flat_over_a_million_rows() {
    let session = postgres();
    let sql = format!("select g, md5(g::text), repeat('x', 100) from generate_series(1, {ROWS}) g");
    let mut handle = block_on(session.execute(&sql, &[])).unwrap();
    let first = block_on(handle.pages.next()).unwrap().unwrap();
    let baseline = resident_bytes();

    // A consumer that stalls must stall the driver, not fill a buffer.
    std::thread::sleep(Duration::from_secs(1));
    let mut peak = resident_bytes();
    let mut rows = first.rows.len();
    while let Some(page) = block_on(handle.pages.next()) {
        rows += page.unwrap().rows.len();
        peak = peak.max(resident_bytes());
    }

    assert_eq!(
        (rows, peak.saturating_sub(baseline) < BUDGET_BYTES),
        (ROWS, true),
        "grew by {} MB",
        peak.saturating_sub(baseline) / (1024 * 1024)
    );
}
