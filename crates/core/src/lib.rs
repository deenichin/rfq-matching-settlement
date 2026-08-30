//! The engine: matching, reservations, and the request/quote state machines.
//!
//! This crate is one half of the seam described in SPEC §13.1. It owns requests, quotes,
//! reservations, claims and the read-only balance *mirror*; it never holds a reference to
//! custody and cannot read a balance from it. The only path out is a [`Event::SubmitIntent`]
//! picked up by an adapter, and the only path back is a command produced by the indexer.
//! `build.rs` enforces the dependency direction at compile time.
//!
//! Stage S0 populates the primitives the rest of the build is written against: the money
//! and time newtypes, the [`Clock`] trait, the generation-counted [`Slab`], and the
//! injectable [`Config`] with its two startup assertions.
//!
//! [`Clock`]: clock::Clock
//! [`Slab`]: slab::Slab
//! [`Config`]: config::Config
//! [`Event::SubmitIntent`]: event::Event::SubmitIntent

pub mod account;
pub mod clock;
pub mod command;
pub mod config;
pub mod engine;
pub mod event;
pub mod ledger;
pub mod quote;
pub mod request;
pub mod reservation;
pub mod slab;
pub mod types;

pub use account::{AccountIdx, MirroredBalance};
pub use clock::{Clock, TestClock};
pub use command::Command;
pub use config::{Config, ConfigError, MAX_LEGS, MAX_QUOTES_PER_LEG};
pub use engine::Engine;
pub use event::Event;
pub use ledger::{InvariantViolation, Ledger, LedgerError, SlabKind};
pub use quote::{Quote, QuoteIdx};
pub use request::{ReqIdx, Request};
pub use reservation::{ResIdx, ResOwner, Reservation};
pub use slab::{Handle, Slab, SlabFull};
pub use types::{Amount, Dur, Price, Size, Ts, UNIT};
