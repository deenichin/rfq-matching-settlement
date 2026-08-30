//! The scenario runner.
//!
//! Named end-to-end scenarios, each printing a readable command/event/balance trace, each
//! running the full chain: matching, acceptance, escrow, resolution, payout. Amounts print
//! as grouped **minor units**, never decimals — a decimal point requires a division, and
//! there is no division in the money path (CLAUDE 15).
//!
//! S0 stands this up as a service so the container harness has all three of its entry
//! points from the first commit. The six scenarios are S7's, and S7 is never cut.

// The only crate in the workspace permitted to write to a terminal. `print_stdout` is
// denied everywhere else, which is how CLAUDE 8 — no I/O in the engine — is enforced by
// the compiler rather than by review.
#![allow(clippy::print_stdout)]

use rfq_core::clock::TestClock;
use rfq_core::config::Config;
use rfq_core::types::Ts;
use rfq_runtime::harness::Harness;

fn main() {
    let config = Config::default();
    // Startup, including the two assertions of SPEC §5.2 and §9.3. `main` is one of the
    // two places CLAUDE 23 permits an unwrap; a venue that cannot validate its own policy
    // must fail loudly and immediately.
    #[allow(clippy::expect_used)]
    let harness = Harness::new(config, TestClock::at(Ts::ZERO), TestClock::at(Ts::ZERO))
        .expect("startup configuration must satisfy SPEC §5.2 and §9.3");

    println!("rfq scenario runner");
    println!("  venue time  {} ms", harness.engine_now().0);
    println!("  chain time  {} ms", harness.custody_now().0);
    println!("  quote ttl   {} ms", config.max_quote_ttl.0);
    println!("  withdrawal  {} ms", harness.custody().withdrawal_delay().0);
    println!();
    println!("no scenarios yet: the six end-to-end traces are stage S7.");
}
