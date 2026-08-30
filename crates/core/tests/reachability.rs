//! S2 gate (g): the state-graph reachability property (SPEC §15.9).
//!
//! Every non-terminal state in the request, quote and contract machines has at least one
//! **reachable** exit transition. Exhaustive and non-randomised: the graph is small enough
//! to enumerate, and a property test over it would only ever rediscover the enumeration.
//!
//! §15.9 declares **two** exceptions, both stuck-by-choice and both argued in their own
//! sections: `Settling` under indefinite `Unknown` (§8.3 — releasing on a guess is the
//! duplication path), and an escrow whose oracle parks in `InProgress` (§10.1 — the oracle's
//! liveness contract). CLAUDE 30 forbids adding a third to make this pass.
//!
//! The assertion is an **equality, not a subset**: the set of non-terminal states with no
//! reachable exit must be exactly what this file declares. A state appearing means a real
//! exit is missing; a state disappearing means the declaration is now too loose. At S2 the
//! set held `Request::Settling`, because `PollSettlement` did not exist yet. S4 supplied
//! that command, so the set is now empty and the test has tightened itself — which is what
//! an equality buys over a subset, and why weakening §15.9 with a third exception to make
//! S2 pass would have been the wrong repair.

#![allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]

use rfq_core::request::{Nonce, RequestState};
use rfq_core::types::Ts;

/// A nonce to build a `Settling` value with. Its contents are irrelevant: this test is about
/// the shape of the graph, not about any particular transaction.
const NONCE: Nonce = Nonce { request: 0, generation: 0 };

/// A state in one of the venue's machines, named for the assertion's message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct StateName(&'static str);

/// The states with no reachable exit.
///
/// **Empty.** Every non-terminal state in every machine implemented so far has at least one
/// transition out of it. Anything appearing here is a real missing exit, and CLAUDE 30
/// forbids adding an entry to make the test pass.
const EXPECTED_WITHOUT_EXIT: [StateName; 0] = [];

/// Every transition the engine can actually perform today, as `(from, to)` pairs.
///
/// Built by hand from the command handlers rather than derived, because a graph derived from
/// the same code it checks would agree with any bug that code contains.
fn implemented_transitions() -> Vec<(StateName, StateName)> {
    vec![
        // Request (§5). `Expired` is derived, not stored, so it is not a node.
        (StateName("Request::Open"), StateName("Request::Rejected")), // RejectRequest
        (StateName("Request::Open"), StateName("Request::Settling")), // AcceptRequest
        // PollSettlement, on a terminal nonce status. `Unknown` and `Pending` move nothing,
        // which is the point of §8.3 rather than a missing edge.
        (StateName("Request::Settling"), StateName("Request::Escrowed")),
        (StateName("Request::Settling"), StateName("Request::SettlementFailed")),
        // Quote (§6).
        (StateName("Quote::Active"), StateName("Quote::Consumed")), // accept, winner
        (StateName("Quote::Active"), StateName("Quote::Released")), // outbid / replaced /
                                                                    // expired at accept /
                                                                    // request rejected
    ]
}

/// Every state, with whether the design calls it terminal.
fn states() -> Vec<(StateName, bool)> {
    vec![
        (StateName("Request::Open"), RequestState::Open.is_terminal()),
        (StateName("Request::Settling"), RequestState::Settling { nonce: NONCE, deadline: Ts(0) }.is_terminal()),
        (StateName("Request::Escrowed"), RequestState::Escrowed.is_terminal()),
        (StateName("Request::Rejected"), RequestState::Rejected.is_terminal()),
        (StateName("Request::SettlementFailed"), RequestState::SettlementFailed.is_terminal()),
        (StateName("Quote::Active"), false),
        (StateName("Quote::Consumed"), true),
        (StateName("Quote::Released"), true),
    ]
}

#[test]
fn every_non_terminal_state_has_a_reachable_exit() {
    let transitions = implemented_transitions();
    let mut without_exit: Vec<StateName> = states()
        .into_iter()
        .filter(|(_, terminal)| !terminal)
        .map(|(state, _)| state)
        .filter(|state| !transitions.iter().any(|(from, _)| from == state))
        .collect();
    without_exit.sort_unstable();

    let mut expected: Vec<StateName> = EXPECTED_WITHOUT_EXIT.to_vec();
    expected.sort_unstable();

    assert_eq!(
        without_exit, expected,
        "the set of non-terminal states with no reachable exit is not what this stage \
         declares. If a state appeared, a real exit is missing and CLAUDE 30 forbids adding \
         an exception to make this pass. If one disappeared, tighten EXPECTED_WITHOUT_EXIT."
    );
}

#[test]
fn settling_now_has_both_of_its_exits_and_the_declared_set_is_empty() {
    // `Settling` leaves by `PollSettlement` on a terminal nonce status: `Settled` to
    // `Escrowed`, `Reverted` to `SettlementFailed`. `Unknown` and `Pending` move nothing,
    // and that is §8.3's policy rather than a missing edge — releasing on a guess is the
    // duplication path.
    assert_eq!(EXPECTED_WITHOUT_EXIT.len(), 0, "no state may be left without an exit");
    assert!(!RequestState::Settling { nonce: NONCE, deadline: Ts(0) }.is_terminal());

    let transitions = implemented_transitions();
    let exits: Vec<StateName> = transitions
        .iter()
        .filter(|(from, _)| *from == StateName("Request::Settling"))
        .map(|(_, to)| *to)
        .collect();
    assert_eq!(
        exits,
        vec![StateName("Request::Escrowed"), StateName("Request::SettlementFailed")]
    );
}

#[test]
fn the_two_declared_exceptions_of_spec_15_9_are_still_exactly_two() {
    // §15.9 names two stuck-by-choice states, both argued in their own sections:
    // `Settling` under *indefinite* `Unknown` (§8.3 — releasing on a guess is the
    // duplication path), and an escrow whose oracle parks in `InProgress` (§10.1 — the
    // oracle's liveness contract).
    //
    // Neither is a missing transition, which is why neither appears above: `Settling` has
    // both its exits, and they are simply not taken while the nonce has no terminal answer.
    // Counting them here is what stops a third being added quietly — CLAUDE 30 forbids it,
    // and a rule nothing checks is a comment.
    const DECLARED_EXCEPTIONS: [&str; 2] = [
        "Settling under indefinite Unknown (SPEC §8.3)",
        "an escrow whose oracle parks in InProgress (SPEC §10.1)",
    ];
    assert_eq!(DECLARED_EXCEPTIONS.len(), 2);

    // The first is a *policy*, not a hole in the graph: the transition exists and the engine
    // declines to take it without a final answer.
    let transitions = implemented_transitions();
    assert!(
        transitions.iter().any(|(from, _)| *from == StateName("Request::Settling")),
        "the first exception is about when the exit is taken, not whether it exists"
    );
}

#[test]
fn every_terminal_state_really_is_terminal() {
    // The other half: a state the design calls terminal must have no outgoing transition. A
    // reachability test that only looked for missing exits would pass on a machine that let
    // a settled request go back to Open.
    let transitions = implemented_transitions();
    for (state, terminal) in states() {
        if terminal {
            assert!(
                !transitions.iter().any(|(from, _)| *from == state),
                "{state:?} is terminal but has an outgoing transition"
            );
        }
    }
}
