//! The scenario stage: a harness, a cast, and a running commentary.
//!
//! Conservation and claim coverage are asserted **after every step**, not at the end. An
//! invariant checked only once tells you the final state is consistent and nothing about how
//! it got there, and the failures this design is about — a claim released twice, an escrow
//! that exists nowhere, capital released on a guess — all show up mid-flight and can net out
//! by the last line.

#![allow(clippy::print_stdout)]

use std::fmt::Write as _;
use rfq_chain::custody::SettleError;
use rfq_core::account::AccountIdx;
use rfq_core::clock::TestClock;
use rfq_core::command::Command;
use rfq_core::config::Config;
use rfq_core::contract::{ContractIdx, Outcome};
use rfq_core::escrow::EscrowId;
use rfq_core::event::Event;
use rfq_core::types::{Amount, Dur, Ts};
use rfq_runtime::harness::{Harness, HarnessError};

use crate::trace::{self, Cast};

/// One scenario's world.
pub(crate) struct Stage {
    harness: Harness<TestClock, TestClock>,
    cast: Cast,
    /// How many events have already been printed.
    printed: usize,
    /// Steps taken, so the trace is numbered and the invariant count is visible.
    steps: u32,
    /// How many times the cross-system invariants have been checked.
    checks: u32,
}

/// Print a heading inside a scenario.
///
/// Free functions rather than methods: they touch no state, and a scenario reads better
/// when the narrative lines are not pretending to be operations on the venue.
pub(crate) fn section(title: &str) {
    println!();
    println!("── {title} ──");
}

/// Print a line of commentary.
pub(crate) fn note(text: &str) {
    println!("   {text}");
}

impl Stage {
    /// Open a scenario.
    ///
    /// # Panics
    ///
    /// If the configuration fails a startup assertion — which would mean the scenario is
    /// describing a venue that must not run.
    #[must_use]
    pub(crate) fn open(title: &str, blurb: &str, config: Config, cast: Cast, start: Ts) -> Self {
        println!();
        println!("═══ {title} ═══");
        println!("{blurb}");
        println!();
        let harness =
            Harness::new(config, TestClock::at(start), TestClock::at(start), TestClock::at(start))
                .unwrap_or_else(|error| panic!("the venue must start: {error:?}"));
        Self { harness, cast, printed: 0, steps: 0, checks: 0 }
    }

    /// The harness, read-only.
    #[must_use]
    pub(crate) const fn harness(&self) -> &Harness<TestClock, TestClock> {
        &self.harness
    }

    /// The harness, mutably — for the parts of a scenario that are not commands.
    pub(crate) const fn harness_mut(&mut self) -> &mut Harness<TestClock, TestClock> {
        &mut self.harness
    }


    /// Move both clocks, and the oracle's with them.
    pub(crate) fn at(&mut self, now: Ts) {
        self.harness.set_both_clocks(now);
        self.harness.oracle_mut().clock_mut().set(now);
    }

    /// Fund an account and confirm it, so the engine's mirror sees it.
    ///
    /// # Panics
    ///
    /// If custody or the engine refuses.
    pub(crate) fn fund(&mut self, account: AccountIdx, amount: Amount) {
        self.harness.deposit(account, amount).unwrap_or_else(|e| panic!("deposit: {e:?}"));
        println!(
            "   fund   {:<10} {:>18}",
            self.cast.name(account),
            trace::minor_units(amount)
        );
        self.check();
    }

    /// Apply a command, print what it did, and check the invariants.
    ///
    /// Returns the engine's answer: a rejection is part of the story, not a failure of it.
    pub(crate) fn apply(&mut self, label: &str, command: Command) -> Result<(), HarnessError> {
        let outcome = self.harness.apply(command);
        self.steps = self.steps.saturating_add(1);
        let verdict = match &outcome {
            Ok(()) => "ok".to_owned(),
            Err(error) => format!("REFUSED {error:?}"),
        };
        println!("{:>4}  {}  {:<44} {}", self.steps, trace::at(self.harness.engine_now()), label, verdict);
        self.print_new_events();
        self.check();
        outcome
    }

    /// Print whatever the engine has emitted since the last time we looked.
    fn print_new_events(&mut self) {
        let emitted = self.harness.emitted();
        for event in emitted.iter().skip(self.printed) {
            println!("        → {}", describe(event, &self.cast));
        }
        self.printed = emitted.len();
    }

    /// Assert every cross-system invariant that holds at this instant.
    ///
    /// # Panics
    ///
    /// On a violation, naming it.
    pub(crate) fn check(&mut self) {
        self.harness.assert_cross_system_invariants();
        self.checks = self.checks.saturating_add(1);
    }

    /// Assert the invariants that hold even mid-settlement, when the engine has not yet
    /// learned what the chain decided.
    ///
    /// # Panics
    ///
    /// On a violation.
    pub(crate) fn check_settlement(&mut self) {
        self.harness.assert_settlement_invariants();
        self.checks = self.checks.saturating_add(1);
    }

    /// Hand every pending bundle to the chain and include it.
    pub(crate) fn settle(&mut self) -> Vec<Result<rfq_chain::custody::SettleReceipt, SettleError>> {
        self.harness.submit_pending();
        println!("      {}  submit bundle", trace::at(self.harness.custody_now()));
        let included = self.harness.include_all();
        for tx in &included {
            let verdict = match &tx.outcome {
                Ok(receipt) => format!("included, {} escrows formed", receipt.n_escrows),
                Err(error) => format!("REVERTED {error:?}"),
            };
            println!("      {}  {verdict}", trace::at(self.harness.custody_now()));
        }
        self.print_new_events();
        self.check_settlement();
        included.into_iter().map(|tx| tx.outcome).collect()
    }

    /// Ask the chain to pay an escrow out, and apply the result.
    ///
    /// # Panics
    ///
    /// If the engine refuses to emit an intent it should have emitted.
    pub(crate) fn settle_escrow(&mut self, escrow: EscrowId, contract: ContractIdx, outcome: Outcome) {
        self.harness.request_escrow_settlement(escrow, contract, outcome);
        self.print_new_events();
        let results = self.harness.apply_payouts();
        for result in results {
            let verdict = match result {
                Ok(true) => "paid".to_owned(),
                Ok(false) => "already settled — a no-op".to_owned(),
                Err(error) => format!("REFUSED {error:?}"),
            };
            println!("      escrow {} {verdict}", escrow.0);
        }
        self.check_settlement();
    }

    /// Print every account's custody balance and the engine's claims against it.
    pub(crate) fn balances(&self, title: &str) {
        println!();
        println!("   {title}");
        println!(
            "   {:<10} {:>18} {:>18} {:>18}",
            "account", "custody balance", "core reserved", "core committed"
        );
        for (account, name) in self.cast.everyone() {
            let balance = self.harness.custody().ledger().balance(*account);
            let entry = self.harness.engine().ledger().account(*account);
            let reserved = entry.map_or(Amount::ZERO, rfq_core::MirroredBalance::reserved);
            let committed = entry.map_or(Amount::ZERO, rfq_core::MirroredBalance::committed);
            println!(
                "   {:<10} {:>18} {:>18} {:>18}",
                name,
                trace::minor_units(balance),
                trace::minor_units(reserved),
                trace::minor_units(committed)
            );
        }
        let locked: usize = self.harness.locked_escrows().count();
        println!("   escrows locked: {locked}");
    }

    /// Resume the indexer and print whatever the engine then learned.
    pub(crate) fn resume_indexer(&mut self) {
        self.harness.resume_indexer();
        println!("      the indexer catches up");
        self.print_new_events();
        self.check_settlement();
    }

    /// Close the scenario with the conservation sum spelled out.
    ///
    /// # Panics
    ///
    /// If the sum does not balance — which is conservation, restated where a reader can
    /// check the arithmetic by eye rather than taking an assertion's word for it.
    pub(crate) fn close(&self) {
        let ledger = self.harness.custody().ledger();
        let mut held = Amount::ZERO;
        for (account, _) in self.cast.everyone() {
            held = Amount(held.0.saturating_add(ledger.balance(*account).0));
        }
        let mut escrowed = Amount::ZERO;
        for (_, escrow) in self.harness.locked_escrows() {
            escrowed = Amount(escrowed.0.saturating_add(escrow.notional().0));
        }
        let expected = Amount(ledger.deposited().0.saturating_sub(ledger.withdrawn().0));
        println!();
        println!(
            "   conservation: balances {} + locked escrows {} = {} , and deposited {} − \
             withdrawn {} = {}",
            trace::minor_units(held),
            trace::minor_units(escrowed),
            trace::minor_units(Amount(held.0.saturating_add(escrowed.0))),
            trace::minor_units(ledger.deposited()),
            trace::minor_units(ledger.withdrawn()),
            trace::minor_units(expected)
        );
        assert_eq!(
            Amount(held.0.saturating_add(escrowed.0)),
            expected,
            "conservation must balance at the end as it did at every step"
        );
        println!("   invariants checked {} times during this scenario", self.checks);
    }
}

/// One event, in words.
fn describe(event: &Event, cast: &Cast) -> String {
    match event {
        Event::RequestOpened { request, deadline, legs, n_legs } => {
            let mut text = format!(
                "RequestOpened request {} deadline {} —",
                request.index(),
                trace::at(*deadline)
            );
            for leg in legs.iter().take(usize::from(*n_legs)) {
                let _ = write!(
                    text,
                    " [contract {} {:?} size {}]",
                    leg.contract.0,
                    leg.side,
                    trace::size(leg.size)
                );
            }
            text.push_str("  (no limit prices — makers never see the reserve)");
            text
        }
        Event::BestSelectionChanged { leg, price, .. } => {
            format!("BestSelectionChanged leg {} now {}", leg.0, trace::price(*price))
        }
        Event::QuoteRejected { quote, maker, reason } => format!(
            "QuoteRejected quote {} to {} — {reason:?}",
            quote.index(),
            cast.name(*maker)
        ),
        Event::QuoteExpired { quote, maker } => {
            format!("QuoteExpired quote {} to {}", quote.index(), cast.name(*maker))
        }
        Event::SubmitIntent { nonce, legs, n_legs, .. } => {
            let mut text = format!("SubmitIntent nonce ({},{}) —", nonce.request, nonce.generation);
            for leg in legs.iter().take(usize::from(*n_legs)) {
                let _ = write!(
                    text,
                    " [contract {} {:?} maker {} @ {}]",
                    leg.contract.0,
                    leg.side,
                    cast.name(leg.maker),
                    trace::price(leg.fill_price)
                );
            }
            text
        }
        Event::RequestEscrowed { request, .. } => {
            format!("RequestEscrowed request {} — custody holds the escrows", request.index())
        }
        Event::RequestSettlementFailed { request, .. } => format!(
            "RequestSettlementFailed request {} — every committed claim returns to free",
            request.index()
        ),
        Event::SettlementStalled { request, status, .. } => format!(
            "SettlementStalled request {} status {status:?} — an alert, and nothing is released",
            request.index()
        ),
        Event::ContractResolved { contract, outcome } => {
            format!("ContractResolved contract {} → {outcome:?}", contract.0)
        }
        Event::SettleIntent { escrow, outcome, .. } => {
            format!("SettleIntent escrow {} under {outcome:?}", escrow.0)
        }
    }
}

/// A duration in whole seconds from a base instant, for a readable timeline.
#[must_use]
pub(crate) fn seconds_after(base: Ts, seconds: u64) -> Ts {
    base.saturating_add(Dur(seconds.saturating_mul(1_000)))
}
