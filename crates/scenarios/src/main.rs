//! The scenario runner — deliverable 2.
//!
//! Six named scenarios, each printing a readable command/event/balance trace. The happy path
//! follows SPEC Appendix A exactly and its figures are asserted against it line by line; a
//! mismatch is a bug in one of them, to be reconciled rather than adjusted.
//!
//! Every scenario runs as far as its story goes. Four run the full chain — matching,
//! acceptance, `Settling`, escrow, resolution, payout. Two are abort paths and end where the
//! abort does, with every unit of capital accounted for: that is the whole content of an
//! abort path, and carrying it further would be describing a different scenario.
//!
//! Conservation and claim coverage are asserted after **every** step, and each scenario
//! prints how many times. Amounts print as grouped minor units, never decimals: a decimal
//! point means dividing by `UNIT`, and there is no division in the money path.

// The only crate in the workspace permitted to write to a terminal. `print_stdout` is denied
// everywhere else, which is how CLAUDE 8 — no I/O in the engine — is enforced by the
// compiler rather than by review.
#![allow(clippy::print_stdout)]
// An `expect` here *is* the assertion: a scenario that cannot take the step it names has
// failed, and must abort the run loudly rather than print a trace of something else. Same
// standing as the test crates, which re-allow this for the same reason.
#![allow(clippy::expect_used)]
// The figures are arithmetic on constants drawn from SPEC Appendix A, checked against it
// line by line. An overflow here would make an assertion compare against nonsense, and the
// assertion is what would catch it — unlike the money path, where CLAUDE 14 applies and
// every operation is checked.
#![allow(clippy::arithmetic_side_effects)]
// A scenario is a linear narrative and is meant to be read top to bottom. Breaking one into
// helpers to satisfy a line count would hide the very ordering the trace exists to show.
#![allow(clippy::too_many_lines)]

mod failures;
mod happy;
mod resolution;
mod stage;
mod trace;

use rfq_core::command::Command;
use rfq_core::config::Config;
use rfq_core::contract::ContractIdx;
use rfq_core::types::{Dur, Ts};

use stage::Stage;

/// A venue whose policy suits a scenario that runs over a minute of venue time and then
/// jumps a day for resolution.
#[must_use]
pub(crate) fn contract_config() -> Config {
    Config {
        max_accounts: 8,
        max_reservations: 64,
        max_requests: 8,
        max_quotes: 64,
        max_contracts: 8,
        max_escrows: 32,
        max_legs: 4,
        max_quotes_per_leg: 4,
        max_quote_ttl: Dur(30_000),
        max_request_ttl: Dur(60_000),
        max_settling_time: Dur(10_000),
        min_horizon: Dur(3_600_000),
        // Covers the wider claim window, max(30s, 60s + 10s) = 70s, with room to spare.
        withdrawal_delay: Dur(200_000),
        challenge_window: Dur(7_200_000),
        stall_grace: Dur(86_400_000),
        escalation_authority: rfq_core::account::AccountIdx(5),
        ..Config::default()
    }
}

/// Far enough out that `now < event_date − MIN_HORIZON` holds for every scenario.
pub(crate) const EVENT_DATE: Ts = Ts(10_000_000);

/// Tell the engine about the contracts a scenario trades.
///
/// # Panics
///
/// If the engine refuses a registration, which would mean the contract table is too small.
pub(crate) fn register_contracts(stage: &mut Stage, contracts: &[ContractIdx]) {
    for contract in contracts {
        stage
            .apply(
                &format!("RegisterContract  contract {}", contract.0),
                Command::RegisterContract { contract: *contract, event_date: EVENT_DATE },
            )
            .expect("a contract registration is always admissible");
    }
}

fn main() {
    println!("RFQ matching and settlement — scenario traces");
    println!("Amounts are minor units; UNIT = 1,000,000 minor units per contract.");
    println!("SPEC Appendix A quotes USDC, so its 65,000 reads as 65,000,000,000 below.");

    happy::run();
    failures::leg_two_of_three_fails();
    failures::settlement_reverts_mid_flight();
    failures::lost_acknowledgement();
    resolution::contested_then_escalated();
    resolution::stalled_into_void();

    println!();
    println!("── all six scenarios complete ──");
    println!("Conservation and claim coverage were asserted after every step of every one.");
}
