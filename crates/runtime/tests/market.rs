//! S2 gate: PLAN (a)–(g), on a **three-leg request with mixed sides** throughout.
//!
//! Three legs because the deliverable names leg 2 of 3; mixed sides because the design's
//! motivating case is a spread — "cuts in September but not in October" — and a request that
//! is long `Yes` on every leg is a parlay, which exercises none of what §2.1 exists for.
//!
//! These drive the engine **synchronously**, one command at a time, so a test can assert
//! between commands and read the handles out of the events the engine just emitted. The
//! threaded path — gateway → channel → engine → ring → publisher → maker feed — is proved
//! end to end in `wiring.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::arithmetic_side_effects)]

use rfq_core::account::AccountIdx;
use rfq_core::command::{Command, ExpectedFill, LegSpec};
use rfq_core::config::{Config, MAX_LEGS};
use rfq_core::contract::ContractIdx;
use rfq_core::engine::{Engine, EngineError};
use rfq_core::event::{Event, EventBuffer, QuoteRejectReason};
use rfq_core::quote::{QuoteIdx, QuoteState};
use rfq_core::request::{ReqIdx, RequestState};
use rfq_core::selection::NoQuoteReason;
use rfq_core::types::{Amount, LegId, Price, Side, Size, Ts, UNIT};

const REQUESTER: AccountIdx = AccountIdx(0);
const ALPHA: AccountIdx = AccountIdx(1);
const BETA: AccountIdx = AccountIdx(2);
const GAMMA: AccountIdx = AccountIdx(3);
const DELTA: AccountIdx = AccountIdx(4);

const SEPTEMBER: ContractIdx = ContractIdx(0);
const OCTOBER: ContractIdx = ContractIdx(1);
const NOVEMBER: ContractIdx = ContractIdx(2);
const EVENT_DATE: Ts = Ts(50_000_000);
const DEADLINE: Ts = Ts(100_000);
const SIZE: Size = Size(100_000);

/// Limits, in minor units per contract. Appendix A's 0.65 and 0.50, plus a third leg.
const LIMIT_A: Price = Price(650_000);
const LIMIT_B: Price = Price(500_000);
const LIMIT_C: Price = Price(400_000);

fn config() -> Config {
    Config {
        max_accounts: 8,
        max_reservations: 64,
        max_requests: 8,
        max_quotes: 64,
        max_contracts: 8,
        max_legs: 4,
        max_quotes_per_leg: 4,
        ..Config::default()
    }
}

/// A synchronous driver: one engine, one event buffer, a clock the test advances by hand.
struct Market {
    engine: Engine,
    events: EventBuffer,
    now: Ts,
    /// Everything the engine has emitted, in order.
    emitted: Vec<Event>,
}

impl Market {
    fn new() -> Self {
        Self {
            engine: Engine::new(config()).unwrap(),
            events: EventBuffer::with_capacity(64),
            now: Ts(1_000),
            emitted: Vec::new(),
        }
    }

    /// Apply one command at the current instant, recording whatever it emitted.
    fn apply(&mut self, command: Command) -> Result<(), EngineError> {
        self.events.clear();
        let outcome = self.engine.apply(command, self.now, &mut self.events);
        let emitted: Vec<Event> = self.events.drain().collect();
        self.emitted.extend(emitted.iter().copied());
        // Every invariant of §15 that the core can see, after every single command.
        assert_eq!(self.engine.ledger().check_invariants(), Ok(()), "after {command:?}");
        assert_eq!(self.engine.ledger().check_claim_coverage(), Ok(()), "after {command:?}");
        outcome
    }

    /// Apply, and require it to succeed.
    fn ok(&mut self, command: Command) {
        self.apply(command).unwrap_or_else(|error| panic!("{command:?} was refused: {error:?}"));
    }

    fn advance(&mut self, to: Ts) {
        assert!(to >= self.now, "the clock does not run backwards");
        self.now = to;
    }

    /// The live quote a maker holds on a leg, found through the read model.
    fn quote_of(&self, request: ReqIdx, leg: LegId, maker: AccountIdx) -> Option<QuoteIdx> {
        self.engine
            .ledger()
            .quotes()
            .find(|(_, quote)| {
                quote.request() == request && quote.leg() == leg && quote.maker() == maker
            })
            .map(|(handle, _)| handle)
    }

    fn account(&self, account: AccountIdx) -> (Amount, Amount, Amount) {
        let entry = self.engine.ledger().account(account).unwrap();
        (entry.free(), entry.reserved(), entry.committed())
    }
}

/// Fund everyone and register the three contracts.
fn open_market() -> Market {
    let mut market = Market::new();
    for account in [REQUESTER, ALPHA, BETA, GAMMA, DELTA] {
        market.ok(Command::CreditAccount { account, free: Amount(1_000_000_000_000) });
    }
    for contract in [SEPTEMBER, OCTOBER, NOVEMBER] {
        market.ok(Command::RegisterContract { contract, event_date: EVENT_DATE });
    }
    market
}

/// The three-leg mixed-side request: September `Yes`, October `No`, November `Yes`.
fn three_leg_request() -> Command {
    let mut legs = [LegSpec::default(); MAX_LEGS];
    legs[0] = LegSpec { contract: SEPTEMBER, side: Side::Yes, size: SIZE, limit: LIMIT_A };
    legs[1] = LegSpec { contract: OCTOBER, side: Side::No, size: SIZE, limit: LIMIT_B };
    legs[2] = LegSpec { contract: NOVEMBER, side: Side::Yes, size: SIZE, limit: LIMIT_C };
    Command::SubmitRequest { requester: REQUESTER, deadline: DEADLINE, legs, n_legs: 3 }
}

/// Open the request and return its handle, read out of the `RequestOpened` event — which is
/// how a client learns it, since a client never holds a slab handle of its own.
fn open_request(market: &mut Market) -> ReqIdx {
    market.ok(three_leg_request());
    market
        .emitted
        .iter()
        .rev()
        .find_map(|event| match event {
            Event::RequestOpened { request, .. } => Some(*request),
            _ => None,
        })
        .expect("SubmitRequest must announce the request")
}

fn quote(
    maker: AccountIdx,
    request: ReqIdx,
    leg: u8,
    price: u32,
    expires_at: Ts,
) -> Command {
    Command::SubmitQuote {
        maker,
        request,
        leg: LegId(leg),
        price: Price(price),
        size: SIZE,
        expires_at,
    }
}

fn accept(request: ReqIdx, prices: [u32; 3]) -> Command {
    let mut expected = [ExpectedFill::default(); MAX_LEGS];
    for (index, price) in prices.iter().enumerate() {
        expected[index] =
            ExpectedFill { leg: LegId(u8::try_from(index).unwrap()), price: Price(*price) };
    }
    Command::AcceptRequest { request, expected, n_legs: 3 }
}

// ═══════════════════════════════ (a) the happy path ═══════════════════════════════

#[test]
fn three_mixed_side_legs_five_quotes_and_a_correct_fill() {
    let mut market = open_market();
    let request = open_request(&mut market);

    // The requester reserved Σ size × limit, before any price existed (§5.2).
    let expected_reservation = Amount(
        SIZE.0 * u64::from(LIMIT_A.0) + SIZE.0 * u64::from(LIMIT_B.0) + SIZE.0 * u64::from(LIMIT_C.0),
    );
    assert_eq!(market.account(REQUESTER).1, expected_reservation);

    // Five quotes: competition on leg 0, one each on legs 1 and 2.
    market.advance(Ts(1_100));
    market.ok(quote(ALPHA, request, 0, 620_000, Ts(1_100 + 20_000)));
    market.advance(Ts(1_200));
    market.ok(quote(BETA, request, 0, 610_000, Ts(1_200 + 20_000)));
    market.advance(Ts(1_300));
    market.ok(quote(GAMMA, request, 0, 610_000, Ts(1_300 + 20_000))); // same price, later
    market.advance(Ts(1_400));
    market.ok(quote(GAMMA, request, 1, 450_000, Ts(1_400 + 20_000)));
    market.advance(Ts(1_500));
    market.ok(quote(ALPHA, request, 2, 380_000, Ts(1_500 + 20_000)));

    let winner_a = market.quote_of(request, LegId(0), BETA).unwrap();
    let winner_b = market.quote_of(request, LegId(1), GAMMA).unwrap();
    let winner_c = market.quote_of(request, LegId(2), ALPHA).unwrap();
    let outbid_alpha = market.quote_of(request, LegId(0), ALPHA).unwrap();
    let outbid_gamma = market.quote_of(request, LegId(0), GAMMA).unwrap();

    // Each maker's contribution is `leg.size × (UNIT − price)`, reserved on admission.
    assert_eq!(market.account(BETA).1, Amount(SIZE.0 * u64::from(UNIT.0 - 610_000)));

    market.advance(Ts(2_000));
    market.ok(accept(request, [610_000, 450_000, 380_000]));

    // The winners are Consumed; the losers are gone entirely, their slots freed.
    assert_eq!(
        market.engine.ledger().quote(winner_a).map(rfq_core::Quote::state),
        Some(QuoteState::Consumed)
    );
    assert_eq!(
        market.engine.ledger().quote(winner_b).map(rfq_core::Quote::state),
        Some(QuoteState::Consumed)
    );
    assert_eq!(
        market.engine.ledger().quote(winner_c).map(rfq_core::Quote::state),
        Some(QuoteState::Consumed)
    );
    assert_eq!(market.engine.ledger().quote(outbid_alpha), None);
    assert_eq!(market.engine.ledger().quote(outbid_gamma), None);

    // Both losers were told, by name, with a reason. Makers are never left inferring the
    // fate of their capital from silence (§7.2).
    let rejected: Vec<(QuoteIdx, AccountIdx)> = market
        .emitted
        .iter()
        .filter_map(|event| match event {
            Event::QuoteRejected { quote, maker, reason: QuoteRejectReason::Outbid } => {
                Some((*quote, *maker))
            }
            _ => None,
        })
        .collect();
    assert_eq!(rejected.len(), 2, "exactly the two outbid quotes on leg 0");
    assert!(rejected.contains(&(outbid_alpha, ALPHA)));
    assert!(rejected.contains(&(outbid_gamma, GAMMA)));

    // Losing reservations are released: Alpha keeps only its winning leg-2 claim.
    let alpha_committed = Amount(SIZE.0 * u64::from(UNIT.0 - 380_000));
    assert_eq!(market.account(ALPHA).1, Amount::ZERO, "Alpha's outbid claim is released");
    assert_eq!(market.account(ALPHA).2, alpha_committed);
    assert_eq!(market.account(GAMMA).1, Amount::ZERO, "Gamma's outbid claim is released");
    assert_eq!(market.account(GAMMA).2, Amount(SIZE.0 * u64::from(UNIT.0 - 450_000)));

    // The requester's over-reservation is released; only the fill is committed.
    let filled =
        Amount(SIZE.0 * (610_000 + 450_000 + 380_000));
    assert_eq!(market.account(REQUESTER).2, filled);
    assert_eq!(market.account(REQUESTER).1, Amount::ZERO);
    assert!(filled < expected_reservation, "the fill beat the limit on every leg");

    // Claim coverage still holds with both sides in `committed` (§15.6).
    assert_eq!(market.engine.ledger().check_claim_coverage(), Ok(()));

    // The request is Settling, and the intent carries the whole bundle — custody cannot
    // reach back into the engine for what it is missing (§13.1).
    let RequestState::Settling(nonce) =
        market.engine.ledger().request(request).unwrap().state()
    else {
        panic!("the request must be Settling");
    };
    let intent = market
        .emitted
        .iter()
        .rev()
        .find(|event| matches!(event, Event::SubmitIntent { .. }))
        .expect("commit emits exactly one intent");
    let Event::SubmitIntent { nonce: emitted_nonce, requester, legs, n_legs, .. } = intent else {
        unreachable!()
    };
    assert_eq!(*emitted_nonce, nonce);
    assert_eq!(*requester, REQUESTER);
    assert_eq!(*n_legs, 3);
    // Mixed sides survive into the bundle — the payout mapping consumes them (§10.3).
    assert_eq!(legs[0].side, Side::Yes);
    assert_eq!(legs[1].side, Side::No);
    assert_eq!(legs[2].side, Side::Yes);
    assert_eq!(legs[0].maker, BETA);
    assert_eq!(legs[0].fill_price, Price(610_000));

    // Escrow does not exist at the end of S2 (§7.2). Nothing here forms one.
    assert!(matches!(
        market.engine.ledger().request(request).unwrap().state(),
        RequestState::Settling(_)
    ));
}

#[test]
fn the_earlier_arrival_wins_a_tie() {
    // Deterministic, and it denies a maker any gain from spamming identical quotes (§7.1).
    let mut market = open_market();
    let request = open_request(&mut market);
    market.advance(Ts(1_100));
    market.ok(quote(BETA, request, 0, 610_000, Ts(30_000)));
    market.advance(Ts(1_200));
    market.ok(quote(GAMMA, request, 0, 610_000, Ts(30_000)));

    let beta = market.quote_of(request, LegId(0), BETA).unwrap();
    market.advance(Ts(1_300));
    market.ok(quote(GAMMA, request, 1, 450_000, Ts(30_000)));
    market.ok(quote(ALPHA, request, 2, 380_000, Ts(30_000)));
    market.advance(Ts(2_000));
    market.ok(accept(request, [610_000, 450_000, 380_000]));

    assert_eq!(
        market.engine.ledger().quote(beta).map(rfq_core::Quote::state),
        Some(QuoteState::Consumed),
        "the earlier arrival at the same price wins"
    );
}

// ══════════════════ (b) leg 2 of 3 fails, and nothing is touched ══════════════════

#[test]
fn leg_two_of_three_has_no_eligible_quote_and_the_whole_request_aborts() {
    let mut market = open_market();
    let request = open_request(&mut market);
    market.advance(Ts(1_100));
    market.ok(quote(BETA, request, 0, 610_000, Ts(30_000)));
    market.ok(quote(ALPHA, request, 2, 380_000, Ts(30_000)));
    // Leg 1 is deliberately left unquoted.

    let leg_zero = market.quote_of(request, LegId(0), BETA).unwrap();
    let leg_two = market.quote_of(request, LegId(2), ALPHA).unwrap();

    market.advance(Ts(2_000));
    // The hash is taken **after normalisation**, which is the baseline SPEC §15.4 states the
    // guarantee against. Normalisation runs before PLAN and is not part of the command's
    // effect, so comparing against a pre-command snapshot would be comparing the wrong thing.
    let mut events = EventBuffer::with_capacity(64);
    let _ = market.engine.apply(
        Command::CreditAccount { account: REQUESTER, free: Amount(1_000_000_000_000) },
        market.now,
        &mut events,
    );
    let before = format!("{:?}", market.engine.ledger());
    let emitted_before = market.emitted.len();

    let outcome = market.apply(accept(request, [610_000, 450_000, 380_000]));
    assert_eq!(
        outcome,
        Err(EngineError::NoEligibleQuote { leg: 1, reason: NoQuoteReason::NoQuotes })
    );

    // Byte-identical to the post-normalisation state.
    assert_eq!(format!("{:?}", market.engine.ledger()), before, "a rejection mutated state");

    // Legs 0 and 2's quotes are still Active — their makers are never told they nearly
    // traded (§7.2). "Provisionally matched" was a local variable inside PLAN.
    assert_eq!(
        market.engine.ledger().quote(leg_zero).map(rfq_core::Quote::state),
        Some(QuoteState::Active)
    );
    assert_eq!(
        market.engine.ledger().quote(leg_two).map(rfq_core::Quote::state),
        Some(QuoteState::Active)
    );
    assert_eq!(market.emitted.len(), emitted_before, "no maker was notified");

    // And the request is still Open, so a later accept may succeed if a quote arrives.
    assert_eq!(market.engine.ledger().request(request).unwrap().state(), RequestState::Open);
}

#[test]
fn the_same_rejection_with_an_expired_reservation_still_compares_equal() {
    // (b2). The requester holds a claim that dies before the accept lands. Normalisation
    // reclaims it, the command still rejects, and the comparison still passes — which is
    // only possible because the baseline is the post-normalisation state. Against a
    // pre-command snapshot this would fail, and the invariant would then get weakened to
    // accommodate it, which is exactly the trap §4.3 names.
    let mut market = open_market();

    // A short-deadline request whose claim expires while a second request is being quoted.
    let mut legs = [LegSpec::default(); MAX_LEGS];
    legs[0] = LegSpec { contract: SEPTEMBER, side: Side::Yes, size: SIZE, limit: LIMIT_A };
    market.ok(Command::SubmitRequest {
        requester: REQUESTER,
        deadline: Ts(1_500),
        legs,
        n_legs: 1,
    });
    let doomed_reservation = market.account(REQUESTER).1;
    assert_ne!(doomed_reservation, Amount::ZERO, "the expiring claim must be non-zero");

    let request = open_request(&mut market);
    market.advance(Ts(1_100));
    market.ok(quote(BETA, request, 0, 610_000, Ts(30_000)));
    market.ok(quote(ALPHA, request, 2, 380_000, Ts(30_000)));

    // Past the short request's deadline: its claim is dead but not yet reclaimed, because
    // nothing has touched the account since.
    market.advance(Ts(2_000));
    assert!(
        market.account(REQUESTER).1 > Amount::ZERO,
        "the dead claim is still on the chain"
    );
    let reserved_before_normalising = market.account(REQUESTER).1;

    // Normalise by hand, then snapshot: this is the baseline.
    market.engine.normalise(REQUESTER, market.now);
    let after_normalisation = format!("{:?}", market.engine.ledger());
    let reserved_after_normalising = market.account(REQUESTER).1;
    assert!(
        reserved_after_normalising < reserved_before_normalising,
        "normalisation must actually have reclaimed something, or (b2) proves nothing"
    );

    let outcome = market.apply(accept(request, [610_000, 450_000, 380_000]));
    assert_eq!(
        outcome,
        Err(EngineError::NoEligibleQuote { leg: 1, reason: NoQuoteReason::NoQuotes })
    );
    assert_eq!(
        format!("{:?}", market.engine.ledger()),
        after_normalisation,
        "the rejection is byte-identical to the POST-normalisation state"
    );
}

// ══════════════════ (c) each leg-failure cause has its own variant ══════════════════

#[test]
fn every_leg_failure_cause_returns_its_own_variant() {
    // §7.2 names four. Three are reachable at selection; the fourth is closed at admission,
    // which is asserted below rather than left as a gap.
    let mut market = open_market();

    // 1. No quotes arrived.
    let request = open_request(&mut market);
    market.advance(Ts(1_100));
    market.ok(quote(BETA, request, 0, 610_000, Ts(30_000)));
    market.ok(quote(GAMMA, request, 1, 450_000, Ts(30_000)));
    market.advance(Ts(1_200));
    assert_eq!(
        market.apply(accept(request, [610_000, 450_000, 380_000])),
        Err(EngineError::NoEligibleQuote { leg: 2, reason: NoQuoteReason::NoQuotes })
    );

    // 2. All quotes expired. Leg 2 gets a quote with a short life.
    market.ok(quote(ALPHA, request, 2, 380_000, Ts(1_500)));
    market.advance(Ts(1_600));
    assert_eq!(
        market.apply(accept(request, [610_000, 450_000, 380_000])),
        Err(EngineError::NoEligibleQuote { leg: 2, reason: NoQuoteReason::AllExpired })
    );

    // 3. Every live quote is priced outside the leg's limit. The quote is **admitted and
    //    reserving capital** — it is excluded at selection, not at admission (§6, §7.1).
    market.ok(quote(DELTA, request, 2, 700_000, Ts(30_000)));
    let over_limit = market.quote_of(request, LegId(2), DELTA).unwrap();
    assert!(
        market.engine.ledger().quote(over_limit).unwrap().claim().is_some(),
        "an over-limit quote reserves capital like any other"
    );
    market.advance(Ts(1_700));
    assert_eq!(
        market.apply(accept(request, [610_000, 450_000, 700_000])),
        Err(EngineError::NoEligibleQuote { leg: 2, reason: NoQuoteReason::OutsideLimit })
    );

    // 4. "No quote covers the full leg size" is closed at ADMISSION, so it can never be a
    //    selection-time cause. §6 lists `size >= leg.size` among the admission checks, which
    //    is why `NoQuoteReason::SizeTooSmall` exists and is unreachable — the variant names
    //    the closure rather than hiding it.
    assert_eq!(
        market.apply(Command::SubmitQuote {
            maker: DELTA,
            request,
            leg: LegId(0),
            price: Price(600_000),
            size: Size(SIZE.0 - 1),
            expires_at: Ts(30_000),
        }),
        Err(EngineError::QuoteTooSmall)
    );
}

// ══════════════ (d) a better quote in flight, and (e) a worse one ══════════════

#[test]
fn a_better_quote_arriving_before_the_accept_fills_at_the_better_price() {
    let mut market = open_market();
    let request = open_request(&mut market);
    market.advance(Ts(1_100));
    market.ok(quote(ALPHA, request, 0, 620_000, Ts(30_000)));
    market.ok(quote(GAMMA, request, 1, 450_000, Ts(30_000)));
    market.ok(quote(ALPHA, request, 2, 380_000, Ts(30_000)));

    // The requester reads 620_000 and starts to accept; Beta improves in flight.
    market.advance(Ts(1_200));
    market.ok(quote(BETA, request, 0, 605_000, Ts(30_000)));
    let beta = market.quote_of(request, LegId(0), BETA).unwrap();

    market.advance(Ts(1_300));
    // Accept carries the *old* view. At-or-better means the better fill is taken silently.
    market.ok(accept(request, [620_000, 450_000, 380_000]));

    assert_eq!(
        market.engine.ledger().quote(beta).map(rfq_core::Quote::state),
        Some(QuoteState::Consumed)
    );
    let filled = Amount(SIZE.0 * (605_000 + 450_000 + 380_000));
    assert_eq!(market.account(REQUESTER).2, filled, "filled at 605_000, not 620_000");
}

#[test]
fn a_worse_selection_at_accept_time_rejects_and_mutates_nothing() {
    // (e). Without this binding the requester is exposed to anything up to their limit, and
    // the limit is a disaster bound rather than a trading decision (§7.1.1).
    let mut market = open_market();
    let request = open_request(&mut market);
    market.advance(Ts(1_100));
    market.ok(quote(BETA, request, 0, 610_000, Ts(1_400)));
    market.ok(quote(GAMMA, request, 1, 450_000, Ts(30_000)));
    market.ok(quote(ALPHA, request, 2, 380_000, Ts(30_000)));
    // A worse quote also stands on leg 0, and outlives the better one.
    market.ok(quote(DELTA, request, 0, 640_000, Ts(30_000)));

    market.advance(Ts(1_500)); // Beta's quote is now dead.
    market.engine.normalise(REQUESTER, market.now);
    let before = format!("{:?}", market.engine.ledger());

    assert_eq!(
        market.apply(accept(request, [610_000, 450_000, 380_000])),
        Err(EngineError::PresentationStale {
            leg: 0,
            expected: Price(610_000),
            actual: Price(640_000)
        })
    );
    assert_eq!(format!("{:?}", market.engine.ledger()), before, "a stale accept mutated state");
    assert_eq!(market.engine.ledger().request(request).unwrap().state(), RequestState::Open);
}

#[test]
fn an_expiring_best_quote_publishes_nothing_and_the_stale_view_fails_safe() {
    // (d3). The feed is eventually consistent by design: normalisation emits nothing, so an
    // expiry-driven change surfaces on the next command touching the request. The requester's
    // view can name a quote that is already dead — and that is safe, not merely tolerated,
    // because the accept binding turns it into `PresentationStale` rather than a bad fill.
    let mut market = open_market();
    let request = open_request(&mut market);
    market.advance(Ts(1_100));
    market.ok(quote(BETA, request, 0, 610_000, Ts(1_400)));
    market.ok(quote(DELTA, request, 0, 640_000, Ts(30_000)));
    market.ok(quote(GAMMA, request, 1, 450_000, Ts(30_000)));
    market.ok(quote(ALPHA, request, 2, 380_000, Ts(30_000)));

    let published_before = market
        .emitted
        .iter()
        .filter(|event| matches!(event, Event::BestSelectionChanged { .. }))
        .count();

    // Time passes and the best quote dies. Nothing is emitted, because nothing is applied.
    market.advance(Ts(1_500));
    let published_after = market
        .emitted
        .iter()
        .filter(|event| matches!(event, Event::BestSelectionChanged { .. }))
        .count();
    assert_eq!(published_before, published_after, "expiry publishes nothing (§4.3)");

    // The requester accepts against what they last saw, and fails safely.
    assert!(matches!(
        market.apply(accept(request, [610_000, 450_000, 380_000])),
        Err(EngineError::PresentationStale { leg: 0, .. })
    ));
}

// ══════════════════ (f) admission never refuses on price ══════════════════

#[test]
fn an_over_limit_quote_is_admitted_reserves_capital_and_is_never_published() {
    // No price-based rejection exists, so there is no free probing channel: a maker cannot
    // bisect the hidden limit by collecting cost-free rejections (§5.2, §6).
    let mut market = open_market();
    let request = open_request(&mut market);
    market.advance(Ts(1_100));

    // 0.70 against a 0.65 limit.
    market.ok(quote(DELTA, request, 0, 700_000, Ts(30_000)));
    let delta = market.quote_of(request, LegId(0), DELTA).unwrap();
    assert!(market.engine.ledger().quote(delta).unwrap().claim().is_some());
    assert_eq!(market.account(DELTA).1, Amount(SIZE.0 * u64::from(UNIT.0 - 700_000)));

    // Never published — an ineligible quote is never selected and never presented, so
    // `BestSelectionChanged` can never carry a price above the leg's limit (§7.1).
    assert!(
        market.emitted.iter().all(|event| !matches!(
            event,
            Event::BestSelectionChanged { price, .. } if *price > LIMIT_A
        )),
        "a price above the leg limit was published"
    );

    // An eligible quote arrives and is published; the over-limit one still is not.
    market.advance(Ts(1_200));
    market.ok(quote(BETA, request, 0, 610_000, Ts(30_000)));
    let published: Vec<Price> = market
        .emitted
        .iter()
        .filter_map(|event| match event {
            Event::BestSelectionChanged { leg: LegId(0), price, .. } => Some(*price),
            _ => None,
        })
        .collect();
    assert_eq!(published, vec![Price(610_000)], "only the eligible quote was ever published");
}

#[test]
fn a_second_quote_from_one_maker_replaces_the_first_and_occupies_one_slot() {
    // (f2). N quotes from one maker occupy one slab slot, not N — which is what bounds
    // occupancy at `makers × legs` rather than by message count (§6).
    let mut market = open_market();
    let request = open_request(&mut market);
    market.advance(Ts(1_100));
    market.ok(quote(ALPHA, request, 0, 640_000, Ts(30_000)));
    let first = market.quote_of(request, LegId(0), ALPHA).unwrap();
    let slots_after_one = market.engine.ledger().quote_count();

    for price in [630_000, 620_000, 610_000] {
        market.advance(Ts(market.now.0 + 10));
        market.ok(quote(ALPHA, request, 0, price, Ts(30_000)));
        assert_eq!(
            market.engine.ledger().quote_count(),
            slots_after_one,
            "requoting must not stack slots"
        );
    }

    assert_eq!(market.engine.ledger().quote(first), None, "the first was released and freed");
    let current = market.quote_of(request, LegId(0), ALPHA).unwrap();
    assert_eq!(market.engine.ledger().quote(current).unwrap().price(), Price(610_000));
    // One claim, for the current price only.
    assert_eq!(market.account(ALPHA).1, Amount(SIZE.0 * u64::from(UNIT.0 - 610_000)));

    // The maker was told each time their own quote was superseded.
    let replaced = market
        .emitted
        .iter()
        .filter(|event| {
            matches!(event, Event::QuoteRejected { reason: QuoteRejectReason::Replaced, .. })
        })
        .count();
    assert_eq!(replaced, 3);
}

#[test]
fn a_quote_at_unit_reserves_nothing_and_is_still_bounded_by_replacement() {
    // (f3). A maker's contribution is `size × (UNIT − price)`, which is **zero** at UNIT.
    // Without the one-quote-per-maker-per-leg rule a zero-capital actor could flood a leg
    // with guaranteed-losing quotes at no cost until `SlabExhausted` starts rejecting honest
    // makers. The channel is closed by **replacement**, not by a capital floor: requoting is
    // admissible at an equal price — "at least as good", not "strictly better" — and each
    // requote frees the slot it replaces, so a hundred messages occupy one slot.
    let mut market = open_market();
    let request = open_request(&mut market);
    market.advance(Ts(1_100));
    market.ok(quote(DELTA, request, 0, UNIT.0, Ts(30_000)));
    assert_eq!(market.account(DELTA).1, Amount::ZERO, "a quote at UNIT reserves nothing");

    let occupied = market.engine.ledger().quote_count();
    for _ in 0..20 {
        market.advance(Ts(market.now.0 + 1));
        market.ok(quote(DELTA, request, 0, UNIT.0, Ts(30_000)));
        assert_eq!(
            market.engine.ledger().quote_count(),
            occupied,
            "twenty free messages must still occupy exactly one slot"
        );
    }
    assert_eq!(market.account(DELTA).1, Amount::ZERO);

    // And the flooder gains nothing by it: each requote is a fresh arrival, so it loses the
    // tiebreak to anyone already standing at the same price.
    market.advance(Ts(market.now.0 + 1));
    market.ok(quote(GAMMA, request, 0, 610_000, Ts(30_000)));
    market.advance(Ts(market.now.0 + 1));
    market.ok(quote(BETA, request, 0, 610_000, Ts(30_000)));
    market.advance(Ts(market.now.0 + 1));
    // Beta requotes at the same price and goes to the back of the queue.
    market.ok(quote(BETA, request, 0, 610_000, Ts(30_000)));
    let gamma = market.quote_of(request, LegId(0), GAMMA).unwrap();

    market.ok(quote(ALPHA, request, 1, 450_000, Ts(30_000)));
    market.ok(quote(ALPHA, request, 2, 380_000, Ts(30_000)));
    market.advance(Ts(market.now.0 + 1));
    market.ok(accept(request, [610_000, 450_000, 380_000]));
    assert_eq!(
        market.engine.ledger().quote(gamma).map(rfq_core::Quote::state),
        Some(QuoteState::Consumed),
        "requoting at the same price forfeits the arrival tiebreak"
    );
}

#[test]
fn a_worse_replacement_is_refused_and_the_standing_quote_survives() {
    // (f4). Without this, replacement is a cancel primitive in disguise: requote at UNIT,
    // which reserves zero, and the maker has withdrawn liquidity they promised was
    // irrevocable (§6).
    let mut market = open_market();
    let request = open_request(&mut market);
    market.advance(Ts(1_100));
    market.ok(quote(ALPHA, request, 0, 610_000, Ts(30_000)));
    let standing = market.quote_of(request, LegId(0), ALPHA).unwrap();
    let reserved = market.account(ALPHA).1;

    market.advance(Ts(1_200));
    assert_eq!(
        market.apply(quote(ALPHA, request, 0, UNIT.0, Ts(30_000))),
        Err(EngineError::WorseReplacement)
    );
    assert_eq!(
        market.apply(quote(ALPHA, request, 0, 620_000, Ts(30_000))),
        Err(EngineError::WorseReplacement)
    );

    // The existing quote stands, at its original price, with its capital still committed.
    assert_eq!(market.engine.ledger().quote(standing).unwrap().price(), Price(610_000));
    assert_eq!(
        market.engine.ledger().quote(standing).unwrap().state(),
        QuoteState::Active
    );
    assert_eq!(market.account(ALPHA).1, reserved);

    // An equal price is admissible — "at least as good", not "strictly better".
    market.ok(quote(ALPHA, request, 0, 610_000, Ts(30_000)));

    // The harm the rule prevents, shown rather than named: a maker who could requote at UNIT
    // would be withdrawing liquidity they promised was irrevocable, because a quote at UNIT
    // reserves zero. With the rule in place Alpha is still on the hook, and the accept below
    // takes their capital at the price they wrote.
    market.advance(Ts(1_300));
    market.ok(quote(GAMMA, request, 1, 450_000, Ts(30_000)));
    market.ok(quote(BETA, request, 2, 380_000, Ts(30_000)));
    market.advance(Ts(1_400));
    market.ok(accept(request, [610_000, 450_000, 380_000]));

    let winner = market.quote_of(request, LegId(0), ALPHA).unwrap();
    assert_eq!(
        market.engine.ledger().quote(winner).unwrap().state(),
        QuoteState::Consumed,
        "the maker could not escape the fill by requoting"
    );
    assert_eq!(
        market.account(ALPHA).2,
        Amount(SIZE.0 * u64::from(UNIT.0 - 610_000)),
        "and their capital was committed at the price they wrote"
    );
}

#[test]
fn a_leg_admits_at_most_max_quotes_per_leg_live_quotes() {
    // (f5). This is what makes the commit phase's event count statically bounded, so
    // `EventBufferFull` is reachable only by construction and never by quote volume (§4.3).
    let mut market = open_market();
    let request = open_request(&mut market);
    market.advance(Ts(1_100));
    let makers = [ALPHA, BETA, GAMMA, DELTA];
    for (index, maker) in makers.iter().enumerate() {
        market.ok(quote(*maker, request, 0, 600_000 + u32::try_from(index).unwrap(), Ts(30_000)));
    }
    assert_eq!(market.engine.ledger().request(request).unwrap().leg(LegId(0)).unwrap().quote_count(), 4);

    // A fifth maker is refused; the bound is on the chain, not on the slab.
    market.ok(Command::CreditAccount { account: AccountIdx(5), free: Amount(1_000_000_000_000) });
    assert_eq!(
        market.apply(quote(AccountIdx(5), request, 0, 600_000, Ts(30_000))),
        Err(EngineError::LegQuoteLimitReached)
    );
    // But an existing maker may still improve — replacement is not a new slot.
    market.advance(Ts(1_200));
    market.ok(quote(ALPHA, request, 0, 590_000, Ts(30_000)));
}

// ══════════════════ the rejected transitions ══════════════════

#[test]
fn cancel_quote_is_a_rejected_transition_not_an_absent_one() {
    // Quotes are irrevocable until expiry in v1; the maker's exposure control is the expiry
    // they chose. Present as a *transition* so enabling it is a policy change, not a
    // redesign (§6, §14) — and an unknown quote is distinguishable from an irrevocable one.
    let mut market = open_market();
    let request = open_request(&mut market);
    market.advance(Ts(1_100));
    market.ok(quote(ALPHA, request, 0, 610_000, Ts(30_000)));
    let standing = market.quote_of(request, LegId(0), ALPHA).unwrap();
    let reserved = market.account(ALPHA).1;

    assert_eq!(
        market.apply(Command::CancelQuote { quote: standing }),
        Err(EngineError::QuotesAreIrrevocable)
    );
    assert_eq!(
        market.engine.ledger().quote(standing).unwrap().state(),
        QuoteState::Active,
        "the quote stands"
    );
    assert_eq!(market.account(ALPHA).1, reserved, "and so does its capital");
}

#[test]
fn rejecting_a_request_releases_every_standing_quote() {
    // An explicit rejection releases every standing quote, so a requester cannot lock maker
    // capital and walk away — the grief costs them the full response deadline, not a
    // keystroke (§11).
    let mut market = open_market();
    let request = open_request(&mut market);
    market.advance(Ts(1_100));
    market.ok(quote(ALPHA, request, 0, 610_000, Ts(30_000)));
    market.ok(quote(BETA, request, 1, 450_000, Ts(30_000)));
    assert_ne!(market.account(ALPHA).1, Amount::ZERO);

    market.advance(Ts(1_200));
    market.ok(Command::RejectRequest { request });

    assert_eq!(market.engine.ledger().request(request).unwrap().state(), RequestState::Rejected);
    assert_eq!(market.account(ALPHA).1, Amount::ZERO, "the maker's capital came back");
    assert_eq!(market.account(BETA).1, Amount::ZERO);
    assert_eq!(market.account(REQUESTER).1, Amount::ZERO, "and so did the requester's");
    let notified = market
        .emitted
        .iter()
        .filter(|event| {
            matches!(
                event,
                Event::QuoteRejected { reason: QuoteRejectReason::RequestRejected, .. }
            )
        })
        .count();
    assert_eq!(notified, 2, "both makers were told");

    // And it is terminal.
    assert_eq!(
        market.apply(Command::RejectRequest { request }),
        Err(EngineError::RequestNotOpen)
    );
}

#[test]
fn admission_refuses_a_deadline_beyond_the_ttl_and_a_contract_too_near_its_event() {
    let mut market = open_market();
    let mut legs = [LegSpec::default(); MAX_LEGS];
    legs[0] = LegSpec { contract: SEPTEMBER, side: Side::Yes, size: SIZE, limit: LIMIT_A };

    assert_eq!(
        market.apply(Command::SubmitRequest {
            requester: REQUESTER,
            deadline: Ts(1_000 + config().max_request_ttl.0 + 1),
            legs,
            n_legs: 1,
        }),
        Err(EngineError::DeadlineTooFar)
    );
    assert_eq!(
        market.apply(Command::SubmitRequest {
            requester: REQUESTER,
            deadline: Ts(500),
            legs,
            n_legs: 1,
        }),
        Err(EngineError::DeadlineInThePast)
    );

    // A contract inside MIN_HORIZON of its event date. Together with §9.3's inequality this
    // is what stops escrow forming on a trade that is immediately Void-resolvable.
    market.ok(Command::RegisterContract { contract: ContractIdx(3), event_date: Ts(1_000_000) });
    legs[0] = LegSpec { contract: ContractIdx(3), side: Side::Yes, size: SIZE, limit: LIMIT_A };
    assert_eq!(
        market.apply(Command::SubmitRequest {
            requester: REQUESTER,
            deadline: DEADLINE,
            legs,
            n_legs: 1,
        }),
        Err(EngineError::ContractTooNear)
    );

    legs[0] = LegSpec { contract: ContractIdx(7), side: Side::Yes, size: SIZE, limit: LIMIT_A };
    assert_eq!(
        market.apply(Command::SubmitRequest {
            requester: REQUESTER,
            deadline: DEADLINE,
            legs,
            n_legs: 1,
        }),
        Err(EngineError::UnknownContract)
    );
}

#[test]
fn a_request_past_its_deadline_is_expired_by_derivation_and_admits_nothing() {
    // `Expired` is derived, never stored (§5): the request is expired iff `now >= deadline`
    // and no accept has been made. Nothing sweeps, so no scheduler timing can decide it.
    let mut market = open_market();
    let mut legs = [LegSpec::default(); MAX_LEGS];
    legs[0] = LegSpec { contract: SEPTEMBER, side: Side::Yes, size: SIZE, limit: LIMIT_A };
    market.ok(Command::SubmitRequest {
        requester: REQUESTER,
        deadline: Ts(1_500),
        legs,
        n_legs: 1,
    });
    let request = market
        .emitted
        .iter()
        .rev()
        .find_map(|event| match event {
            Event::RequestOpened { request, .. } => Some(*request),
            _ => None,
        })
        .unwrap();

    market.advance(Ts(1_500));
    assert!(market.engine.ledger().request(request).unwrap().is_expired_at(market.now));
    assert_eq!(
        market.engine.ledger().request(request).unwrap().state(),
        RequestState::Open,
        "expiry is derived, so the stored state is untouched"
    );
    assert_eq!(
        market.apply(quote(ALPHA, request, 0, 610_000, Ts(30_000))),
        Err(EngineError::RequestDeadlinePassed)
    );
    assert_eq!(
        market.apply(accept(request, [610_000, 0, 0])),
        Err(EngineError::RequestDeadlinePassed)
    );

    // The requester's capital comes back on the next command touching the account.
    market.ok(Command::CreditAccount { account: REQUESTER, free: Amount(1_000_000_000_000) });
    assert_eq!(market.account(REQUESTER).1, Amount::ZERO);
}
