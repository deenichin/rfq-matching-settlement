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
//! # The stage gap this test reports rather than hides
//!
//! At S2, `Settling` has **no** reachable exit — not "no exit under indefinite `Unknown`",
//! but none at all, because `PollSettlement` is S4's command and nothing else can move a
//! request out of it. Committed capital therefore has no exit either.
//!
//! That is a stage artifact, not a design defect, and the honest way to encode it is to
//! assert that the set of exit-less non-terminal states is **exactly** what this stage
//! expects. So the assertion below is an equality, not a subset: if any *other* state loses
//! its exit the test fails, and when S4 lands, `EXPECTED_WITHOUT_EXIT` becomes empty and the
//! test tightens itself. Weakening §15.9 with a third exception would have made this pass
//! and made the specification false.

#![allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]

use rfq_core::request::RequestState;

/// A state in one of the venue's machines, named for the assertion's message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct StateName(&'static str);

/// The states with no reachable exit **at this stage**, and why.
///
/// Empty from S4 onwards. Anything appearing here that is not listed is a real missing exit.
const EXPECTED_WITHOUT_EXIT: [StateName; 1] = [StateName("Request::Settling")];

/// Every transition the engine can actually perform today, as `(from, to)` pairs.
///
/// Built by hand from the command handlers rather than derived, because a graph derived from
/// the same code it checks would agree with any bug that code contains.
fn implemented_transitions() -> Vec<(StateName, StateName)> {
    vec![
        // Request (§5). `Expired` is derived, not stored, so it is not a node.
        (StateName("Request::Open"), StateName("Request::Rejected")), // RejectRequest
        (StateName("Request::Open"), StateName("Request::Settling")), // AcceptRequest
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
        (
            StateName("Request::Settling"),
            RequestState::Settling(rfq_core::request::Nonce { request: 0, generation: 0 })
                .is_terminal(),
        ),
        (StateName("Request::Escrowed"), RequestState::Escrowed.is_terminal()),
        (StateName("Request::Rejected"), RequestState::Rejected.is_terminal()),
        (StateName("Request::SettlementFailed"), RequestState::SettlementFailed.is_terminal()),
        (StateName("Quote::Active"), false),
        (StateName("Quote::Consumed"), true),
        (StateName("Quote::Released"), true),
    ]
}

#[test]
fn every_non_terminal_state_has_a_reachable_exit_except_the_ones_this_stage_declares() {
    let transitions = implemented_transitions();
    let mut without_exit: Vec<StateName> = states()
        .into_iter()
        .filter(|(_, terminal)| !terminal)
        .map(|(state, _)| state)
        .filter(|state| !transitions.iter().any(|(from, _)| from == state))
        .collect();
    without_exit.sort_unstable();

    let mut expected = EXPECTED_WITHOUT_EXIT.to_vec();
    expected.sort_unstable();

    assert_eq!(
        without_exit, expected,
        "the set of non-terminal states with no reachable exit is not what this stage \
         declares. If a state appeared, a real exit is missing and CLAUDE 30 forbids adding \
         an exception to make this pass. If one disappeared, tighten EXPECTED_WITHOUT_EXIT."
    );
}

#[test]
fn the_declared_gap_is_settling_and_it_closes_in_s4() {
    // Stated as its own assertion so the gap is impossible to read past. `Settling` exits by
    // `PollSettlement` to `Escrowed` or `SettlementFailed` (§5), and that command belongs to
    // S4. Until then a request that has been accepted stays there, and both sides' capital
    // stays `committed` — which is the *correct* behaviour for an unresolved settlement
    // (§8.3: releasing on a guess is the duplication path), just not yet the complete one.
    assert_eq!(EXPECTED_WITHOUT_EXIT.len(), 1);
    assert_eq!(EXPECTED_WITHOUT_EXIT[0], StateName("Request::Settling"));

    // Both §15.9 exceptions are narrower than this gap: they are about *indefinite*
    // `Unknown` and a parked oracle, not about the transition being absent. Neither of them
    // covers what this stage is missing, and neither has been stretched to.
    assert!(
        !RequestState::Settling(rfq_core::request::Nonce { request: 0, generation: 0 })
            .is_terminal(),
        "Settling is not terminal; it is unfinished"
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
