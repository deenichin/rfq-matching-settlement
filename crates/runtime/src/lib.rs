//! The wiring between the two systems.
//!
//! `rfq-core` and `rfq-chain` share no memory and cannot see each other (SPEC §13.1).
//! This crate is where they are placed side by side: it samples the engine's clock at the
//! call site and hands the value to `apply`, it pumps engine events to the settlement
//! adapter and the chain log to the indexer, and it owns the [`Harness`] that holds both.
//!
//! Stage S0 delivers the clock implementations and the harness skeleton. The command
//! channel and publisher thread land in S1.5, the settlement adapter in S3.
//!
//! [`Harness`]: harness::Harness

pub mod clock;
pub mod event_ring;
pub mod gateway;
pub mod harness;
pub mod venue;

pub use clock::MonotonicClock;
pub use event_ring::{EventRing, Sequenced, SequencedEvent};
pub use gateway::{ContractRef, ContractRegistry, Gateway, GatewayError};
pub use harness::Harness;
pub use venue::{EventSink, LogEntry, NullSink, RoundOutcome, RuntimeCapacities, Venue, replay};
