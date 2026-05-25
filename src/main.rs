//! Thin CLI shim. The real entry point is `mezame::run()`; the crate is
//! split into a library so tests in `tests/` can reach internals.

use anyhow::Result;

fn main() -> Result<()> {
    mezame::run()
}
