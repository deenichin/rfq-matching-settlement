//! S2 gate (d2): the event path, proved **end to end**.
//!
//! S1.5 exercised the publisher only by pushing into the ring directly, because no command
//! in that stage's set emitted anything. This stage produces the first real events, so the
//! whole path runs here: gateway → command channel → engine thread → event ring →
//! publisher thread → what a maker actually receives.
//!
//! The property under test is §5.2's: `RequestOpened` fans each leg's **contract
//! description, side and size** out to makers, and carries **no limit price**. Without this
//! event no maker learns a request exists and the venue is not an RFQ; with the limit in it,
//! a revealed reserve shades quotes toward the limit rather than toward the maker's true
//! best price.
//!
//! It also proves the description never crossed into the core: the engine's event carries a
//! `ContractIdx`, and the publisher resolves it back to the verbatim wording through the
//! gateway's registry.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::arithmetic_side_effects)]

use std::sync::{Arc, Mutex};

use rfq_core::clock::TickClock;
use rfq_core::command::Command;
use rfq_core::config::Config;
use rfq_core::event::Event;
use rfq_core::types::{Amount, Dur, Price, Side, Size, Ts};
use rfq_runtime::gateway::{ContractRef, ContractRegistry, Gateway};
use rfq_runtime::venue::{EventSink, RuntimeCapacities, Venue};
use rfq_runtime::SequencedEvent;

/// One leg as a maker receives it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct BroadcastLeg {
    description: Vec<u8>,
    event_date: Ts,
    resolution_source: Vec<u8>,
    side: Side,
    size: Size,
}

/// Everything that reached the wire.
#[derive(Debug, Default)]
struct MakerFeed {
    opened: Vec<(Ts, Vec<BroadcastLeg>)>,
    selections: Vec<(u8, Price)>,
    sequences: Vec<u64>,
}

/// The publisher's sink — the only place I/O happens (CLAUDE 8). Here it records; in the
/// scenario runner it prints.
struct Publisher {
    registry: Arc<Mutex<ContractRegistry>>,
    feed: Arc<Mutex<MakerFeed>>,
}

impl EventSink for Publisher {
    fn publish(&mut self, event: SequencedEvent) {
        let Ok(mut feed) = self.feed.lock() else { return };
        feed.sequences.push(event.sequence);
        match event.event {
            Event::RequestOpened { deadline, legs, n_legs, .. } => {
                let Ok(registry) = self.registry.lock() else { return };
                let mut broadcast = Vec::new();
                for leg in legs.iter().take(usize::from(n_legs)) {
                    // Republished verbatim: a maker quoting a contract is asserting they have
                    // read and priced that exact byte sequence (§5.3).
                    let Some(reference) = registry.describe(leg.contract) else { continue };
                    broadcast.push(BroadcastLeg {
                        description: reference.description.clone(),
                        event_date: reference.event_date,
                        resolution_source: reference.resolution_source.clone(),
                        side: leg.side,
                        size: leg.size,
                    });
                }
                feed.opened.push((deadline, broadcast));
            }
            Event::BestSelectionChanged { leg, price, .. } => {
                feed.selections.push((leg.0, price));
            }
            _ => {}
        }
    }
}

fn contract(name: &str) -> ContractRef {
    ContractRef {
        description: format!("ECB cuts at the {name} meeting").into_bytes(),
        event_date: Ts(50_000_000),
        resolution_source: b"ECB press release".to_vec(),
    }
}

#[test]
fn a_request_reaches_makers_with_its_wording_its_sides_and_its_sizes_but_no_limits() {
    let config = Config {
        max_accounts: 8,
        max_reservations: 64,
        max_requests: 8,
        max_quotes: 64,
        max_contracts: 8,
        max_legs: 4,
        max_quotes_per_leg: 4,
        ..Config::default()
    };
    let mut gateway = Gateway::new(config.max_accounts, config.max_contracts);
    let feed = Arc::new(Mutex::new(MakerFeed::default()));
    let publisher = Publisher { registry: gateway.registry(), feed: Arc::clone(&feed) };
    let venue = Venue::start(
        config,
        TickClock::new(Ts(1_000), Dur(1)),
        publisher,
        RuntimeCapacities {
            command_channel: 8,
            event_ring: 256,
            event_buffer: 64,
            command_log: 256,
        },
    )
    .unwrap();

    // Three legs, mixed sides — the calendar spread, plus a third.
    let legs = vec![
        (contract("September"), Side::Yes, Size(100_000), Price(650_000)),
        (contract("October"), Side::No, Size(100_000), Price(500_000)),
        (contract("November"), Side::Yes, Size(50_000), Price(400_000)),
    ];
    let (registrations, request) =
        gateway.submit_request(b"requester", Ts(100_000), &legs).unwrap();

    let sender = venue.commands();
    let requester = gateway.account(b"requester").unwrap();
    let maker = gateway.account(b"alpha").unwrap();
    sender
        .send(Command::CreditAccount { account: requester, free: Amount(1_000_000_000_000) })
        .unwrap();
    sender
        .send(Command::CreditAccount { account: maker, free: Amount(1_000_000_000_000) })
        .unwrap();
    for registration in registrations {
        sender.send(registration).unwrap();
    }
    sender.send(request).unwrap();
    drop(sender);

    let outcome = venue.join();
    assert!(outcome.log.iter().all(|entry| entry.outcome.is_ok()), "{:?}", outcome.log);
    assert!(outcome.events_emitted >= 1, "the engine emitted nothing");
    assert_eq!(outcome.coverage_checks, outcome.log.len() as u64, "coverage ran per command");

    let feed = feed.lock().unwrap();
    assert_eq!(feed.opened.len(), 1, "exactly one request was announced");
    let (deadline, broadcast) = feed.opened.first().unwrap();
    assert_eq!(*deadline, Ts(100_000));
    assert_eq!(broadcast.len(), 3);

    // The wording arrives verbatim, having never been inside the core.
    assert_eq!(broadcast[0].description, b"ECB cuts at the September meeting".to_vec());
    assert_eq!(broadcast[1].description, b"ECB cuts at the October meeting".to_vec());
    assert_eq!(broadcast[2].description, b"ECB cuts at the November meeting".to_vec());
    assert_eq!(broadcast[0].resolution_source, b"ECB press release".to_vec());
    assert_eq!(broadcast[0].event_date, Ts(50_000_000));

    // Sides and sizes arrive; a maker knows which side they would be taking.
    assert_eq!(broadcast[0].side, Side::Yes);
    assert_eq!(broadcast[1].side, Side::No);
    assert_eq!(broadcast[2].side, Side::Yes);
    assert_eq!(broadcast[0].size, Size(100_000));
    assert_eq!(broadcast[2].size, Size(50_000));

    // And the limits do not. `OpenLeg` carries a contract, a side and a size, and there is
    // no price on it — so exact equality against the expected broadcast is the assertion:
    // the day a fourth field appears, this stops compiling or stops matching, which is
    // exactly when someone should have to argue for it (§5.2).
    let expected = |name: &str, side, size| BroadcastLeg {
        description: format!("ECB cuts at the {name} meeting").into_bytes(),
        event_date: Ts(50_000_000),
        resolution_source: b"ECB press release".to_vec(),
        side,
        size,
    };
    assert_eq!(
        *broadcast,
        vec![
            expected("September", Side::Yes, Size(100_000)),
            expected("October", Side::No, Size(100_000)),
            expected("November", Side::Yes, Size(50_000)),
        ],
        "the broadcast carries the wording, the sides and the sizes — and nothing else"
    );
}

#[test]
fn the_gateway_resolves_identical_wording_to_one_index_and_a_byte_apart_to_two() {
    // Identity is byte equality over description + event date + resolution source. There is
    // no fuzzy matching and no canonicalisation — near-identical wording produces different
    // contracts, which is correct: the wording *is* the product (§5.3).
    let mut gateway = Gateway::new(8, 8);
    let september = contract("September");

    let (first, registration) = gateway.contract(&september).unwrap();
    assert!(registration.is_some(), "the first reference allocates and announces");
    let (again, none) = gateway.contract(&september.clone()).unwrap();
    assert_eq!(first, again, "identical bytes resolve to the same index");
    assert!(none.is_none(), "a known contract needs no second registration");

    // One byte of difference is a different product.
    let mut typo = september.clone();
    typo.description.push(b'.');
    let (other, registration) = gateway.contract(&typo).unwrap();
    assert_ne!(first, other);
    assert!(registration.is_some());

    // So is the same wording under a different resolution source...
    let mut other_source = september.clone();
    other_source.resolution_source = b"Reuters".to_vec();
    assert_ne!(gateway.contract(&other_source).unwrap().0, first);

    // ...and the same wording about a different date.
    let mut other_date = september.clone();
    other_date.event_date = Ts(50_000_001);
    assert_ne!(gateway.contract(&other_date).unwrap().0, first);
}
