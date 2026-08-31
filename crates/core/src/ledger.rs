//! The ledger: the balance mirror, the reservation slab, and the two chains (SPEC §4.3).
//!
//! This is where the money model of §2.2 and §2.4 becomes code. Four operations move a
//! claim, and every one of them asserts the chain-sum invariants afterwards:
//!
//! | Operation | Effect | Cost |
//! |---|---|---|
//! | [`Ledger::reserve`] | `free → reserved`; link into the account's expiry chain | walk back from the tail, near-O(1) |
//! | [`Ledger::release`] | `reserved → free`; unlink | O(1) |
//! | [`Ledger::commit`] | `reserved → committed`; unlink from one chain and link into the other, in one step | O(1) |
//! | [`Ledger::release_expired`] | bulk `reserved → free` from the head | O(number actually reclaimed) |
//!
//! **Why the ledger owns the request and quote slabs.** Invariant 3 (§15.3) is a statement
//! about a bidirectional link: every claim's owner must resolve, *and* the resolved target
//! must point back at the claim. A ledger that could not reach the owner could only check
//! half of it, and the half it could check is the half that never catches anything. So the
//! back-pointer is maintained here, in one place, rather than by every caller — the whole
//! point of the invariant is that callers get this wrong.
//!
//! Nothing in this module allocates after construction (CLAUDE 9). Slabs are preallocated
//! to configured capacity; the chains are intrusive index links; exhaustion is
//! [`LedgerError::SlabExhausted`], never a grow.

use crate::account::{AccountIdx, Link, MirroredBalance};
use crate::config::Config;
use crate::contract::{Contract, ContractIdx};
use crate::quote::{Quote, QuoteIdx, QuoteState};
use crate::request::{ReqIdx, Request, RequestState};
use crate::reservation::{ClaimLinks, ResIdx, ResOwner, Reservation};
use crate::slab::Slab;
use crate::types::{Amount, Ts};

/// Which preallocated slab ran out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlabKind {
    /// The reservation slab.
    Reservation,
    /// The request slab.
    Request,
    /// The quote slab.
    Quote,
}

/// A rejected ledger operation. One variant per cause (CLAUDE 24).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LedgerError {
    /// The account index is outside the preallocated mirror.
    UnknownAccount,
    /// A preallocated slab is full. A rejection, never a reallocation (§4.3, CLAUDE 10).
    SlabExhausted {
        /// Which slab.
        slab: SlabKind,
    },
    /// The handle's generation no longer matches: the claim it named has been released and
    /// its slot possibly reissued.
    ///
    /// Distinct from [`LedgerError::ReservationCommitted`] on purpose. A stale handle means
    /// the caller is holding a reference past its lifetime; a committed handle means the
    /// caller is trying to release capital that may not be released on a guess. Those are
    /// different bugs in the caller and must not share a variant (§4.3).
    StaleReservation,
    /// [`Ledger::release`] was called on committed capital.
    ///
    /// The guard belongs here and **only** here. `release_expired` needs no equivalent:
    /// committed claims are not on the expiry chain, so its traversal cannot reach one. A
    /// guard there would be a filter that can be mis-scoped; its absence is the proof.
    ReservationCommitted,
    /// The owner handle does not resolve — the quote or request has been freed.
    StaleOwner,
    /// The owner already holds a claim. A quote backs exactly one reservation, and a
    /// request backs exactly one requester-side reservation (§2.4).
    OwnerAlreadyClaimed,
    /// An owner was closed while capital is still claimed against it.
    ///
    /// Freeing the slot would leave a claim naming a reissued owner — the stale-handle
    /// class invariant 3 exists to catch. Refused here so the assertion never has to.
    OwnerStillClaimed,
    /// The request handle does not resolve.
    StaleRequest,
    /// The claim would draw on capital the mirrored balance does not cover, breaking claim
    /// coverage (§15.6) before custody ever sees it.
    InsufficientFree,
    /// Checked arithmetic on a total overflowed. A rejection, never a wrap (CLAUDE 14).
    AmountOverflow,
    /// The contract index is outside the preallocated table, or was never registered.
    UnknownContract,
    /// A contract index was re-registered with a different event date. The event date is
    /// part of the contract's identity (§5.3), so this is the gateway contradicting itself.
    ContractIdentityChanged,
}

/// A broken structural invariant (SPEC §15.1–§15.3).
///
/// Returned rather than panicked so a test can assert *which* invariant broke; the ledger
/// itself turns this into a `debug_assert` after every mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvariantViolation {
    /// §15.1 — `account.reserved` is not the sum over that account's expiry chain.
    ReservedTotalMismatch(AccountIdx),
    /// §15.1 — `account.committed` is not the sum of that account's entries across the
    /// committed lists.
    CommittedTotalMismatch(AccountIdx),
    /// The expiry chain is not ascending by `expires_at`.
    ExpiryChainOutOfOrder(AccountIdx),
    /// A `next`/`prev` pair disagrees, or an endpoint is wrong.
    ChainLinksInconsistent(AccountIdx),
    /// A committed claim was found on an expiry chain. Two chains, never both (§4.3).
    CommittedClaimOnExpiryChain(AccountIdx),
    /// A claim on the chain belongs to a different account.
    ClaimOnForeignChain(AccountIdx),
    /// A reserved claim was found on a request's committed list.
    ReservedClaimOnCommittedList(ReqIdx),
    /// A committed claim is on a list belonging to a different request.
    ClaimOnForeignCommittedList(ReqIdx),
    /// §15.3 — a claim's owner does not resolve. **Not** "nothing to check": this is
    /// exactly the stale-handle case the generation counters exist to catch, and treating
    /// a failed dereference as a pass would blind the assertion to it (CLAUDE 42).
    OwnerDoesNotResolve(ResIdx),
    /// §15.3 — the owner resolves but does not point back at this claim.
    OwnerDoesNotPointBack(ResIdx),
    /// A live claim is on no chain, or on two.
    ClaimNotOnExactlyOneChain,
    /// §15.2 — a claim with `expires_at <= now` survived normalisation of its account.
    ExpiredClaimAfterNormalisation(AccountIdx),
    /// §15.6 — the mirrored balance no longer covers the claims against it. The engine has
    /// promised capital custody does not hold.
    ClaimCoverageBroken(AccountIdx),
    /// §15.3 — a `reserved` claim's owner is not standing: a quote that is no longer
    /// `Active`, or a request that is no longer `Open`.
    ReservedClaimOwnerNotStanding(ResIdx),
    /// §15.3 — a `committed` claim names a quote that is not `Consumed`.
    CommittedClaimOwnerNotConsumed(ResIdx),
    /// §15.3 — a `committed` claim hangs off a request that is still `Open`, so capital is
    /// in the in-flight bucket with nothing in flight (§2.4).
    CommittedClaimRequestNotSettling(ResIdx),
}

/// The engine's claim ledger.
///
/// `PartialEq` is structural, down to slab generations and free-list order, so replaying a
/// command log through a fresh ledger and comparing is the byte-for-byte assertion SPEC §13
/// asks for rather than a summary that could agree by coincidence.
#[derive(Debug, PartialEq, Eq)]
pub struct Ledger {
    accounts: Vec<MirroredBalance>,
    /// Dense, never freed, so no generation — the same argument as accounts (§5.3).
    /// `None` means the gateway has not registered that index yet.
    contracts: Vec<Option<Contract>>,
    reservations: Slab<Reservation>,
    requests: Slab<Request>,
    quotes: Slab<Quote>,
}

impl Ledger {
    /// Preallocate everything to configured capacity. The only allocation the ledger
    /// performs (SPEC §3, CLAUDE 9).
    #[must_use]
    pub fn new(config: &Config) -> Self {
        let mut accounts = Vec::with_capacity(config.max_accounts as usize);
        accounts.resize(config.max_accounts as usize, MirroredBalance::default());
        let mut contracts = Vec::with_capacity(config.max_contracts as usize);
        contracts.resize(config.max_contracts as usize, None);
        Self {
            accounts,
            contracts,
            reservations: Slab::with_capacity(config.max_reservations),
            requests: Slab::with_capacity(config.max_requests),
            quotes: Slab::with_capacity(config.max_quotes),
        }
    }

    // ─────────────────────────── the balance mirror (§2.3) ───────────────────────────

    /// How many accounts the mirror holds. Fixed at construction; accounts are never freed.
    #[must_use]
    pub fn account_count(&self) -> u32 {
        u32::try_from(self.accounts.len()).unwrap_or(u32::MAX)
    }

    /// One account's mirrored balance and claim totals.
    #[must_use]
    pub fn account(&self, account: AccountIdx) -> Option<&MirroredBalance> {
        self.accounts.get(account.0 as usize)
    }

    /// Apply a mirror update.
    ///
    /// The core never *computes* a balance — this is the write path for a chain event
    /// arriving through the indexer (§12), and the mirror is a projection, not an
    /// authority (§2.3). Named for what it is so that no engine path is tempted to call it.
    ///
    /// # Errors
    ///
    /// [`LedgerError::UnknownAccount`] if the index is outside the preallocated mirror.
    pub fn apply_mirror_update(
        &mut self,
        account: AccountIdx,
        free: Amount,
    ) -> Result<(), LedgerError> {
        let entry = self.accounts.get_mut(account.0 as usize).ok_or(LedgerError::UnknownAccount)?;
        entry.set_free(free);
        Ok(())
    }

    /// What `account` may draw on **after normalising it** — `free − reserved − committed`
    /// with every expired claim already reclaimed.
    ///
    /// Not to be confused with `ledger.account(a).free()`, which is the *mirrored balance*:
    /// reserving moves no money, so the mirrored balance still contains every unit a claim
    /// refers to (§2.2). This is the spare part, and it is the only one admission may spend.
    ///
    /// This is the admission query, and normalising first is the correctness path: a
    /// decision about A is never taken without reclaiming A's expired capital, so the
    /// answer can never be stale (§4.3).
    ///
    /// # Errors
    ///
    /// [`LedgerError::UnknownAccount`].
    pub fn free(&mut self, account: AccountIdx, now: Ts) -> Result<Amount, LedgerError> {
        self.release_expired(account, now)?;
        self.account(account).map(MirroredBalance::available).ok_or(LedgerError::UnknownAccount)
    }

    // ────────────────────────────── owners (§15.3) ──────────────────────────────

    /// Record a contract the gateway has assigned an index to (§5.3).
    ///
    /// Re-registering the same index with the same event date is a no-op, because the
    /// gateway resolves an identical description to the same index; re-registering it with a
    /// *different* event date is refused, since the event date is part of the identity.
    ///
    /// # Errors
    ///
    /// [`LedgerError::UnknownContract`] if the index is outside the preallocated table,
    /// [`LedgerError::ContractIdentityChanged`] if it is already registered differently.
    pub fn register_contract(
        &mut self,
        contract: ContractIdx,
        event_date: Ts,
    ) -> Result<(), LedgerError> {
        let entry =
            self.contracts.get_mut(contract.0 as usize).ok_or(LedgerError::UnknownContract)?;
        match entry {
            Some(existing) if existing.event_date() == event_date => Ok(()),
            Some(_) => Err(LedgerError::ContractIdentityChanged),
            None => {
                *entry = Some(Contract::new(event_date));
                Ok(())
            }
        }
    }

    /// A registered contract, if the index resolves.
    #[must_use]
    pub fn contract(&self, contract: ContractIdx) -> Option<&Contract> {
        self.contracts.get(contract.0 as usize).and_then(Option::as_ref)
    }

    /// Record what the oracle says about a contract. Monotonicity is checked by the caller.
    pub(crate) fn set_oracle_status(
        &mut self,
        contract: ContractIdx,
        status: crate::contract::OracleStatus,
    ) {
        if let Some(Some(entry)) = self.contracts.get_mut(contract.0 as usize) {
            entry.set_oracle_status(status);
        }
    }

    /// How many contract indices the table is preallocated for.
    #[must_use]
    pub fn contract_capacity(&self) -> u32 {
        u32::try_from(self.contracts.len()).unwrap_or(u32::MAX)
    }

    /// Free a quote slot.
    ///
    /// S2 calls this when a quote leaves `Active`. It is here in S1 because a slab whose
    /// entries are never freed is not a slab, and because invariant 3's "the owner **must
    /// resolve**" half is only meaningful where an owner can stop existing.
    ///
    /// # Errors
    ///
    /// [`LedgerError::StaleOwner`] if the handle does not resolve;
    /// [`LedgerError::OwnerStillClaimed`] if capital is still claimed against it. Release
    /// the claim first — the ordering is the point.
    pub fn close_quote(&mut self, quote: QuoteIdx) -> Result<(), LedgerError> {
        let held = self.quotes.get(quote).ok_or(LedgerError::StaleOwner)?.claim();
        if held.is_some() {
            return Err(LedgerError::OwnerStillClaimed);
        }
        self.quotes.remove(quote);
        self.assert_invariants();
        Ok(())
    }

    /// Free a request slot.
    ///
    /// # Errors
    ///
    /// [`LedgerError::StaleRequest`], or [`LedgerError::OwnerStillClaimed`] if the
    /// requester's claim is still held or the committed list is not empty. Committed
    /// capital outlives the reservation phase and may not be released on a guess (§8.3), so
    /// a request holding any cannot be freed.
    pub fn close_request(&mut self, request: ReqIdx) -> Result<(), LedgerError> {
        let entry = self.requests.get(request).ok_or(LedgerError::StaleRequest)?;
        if entry.claim().is_some() || entry.committed_head.index().is_some() {
            return Err(LedgerError::OwnerStillClaimed);
        }
        self.requests.remove(request);
        self.assert_invariants();
        Ok(())
    }

    /// A request, if the handle resolves.
    #[must_use]
    pub fn request(&self, request: ReqIdx) -> Option<&Request> {
        self.requests.get(request)
    }

    /// A quote, if the handle resolves.
    #[must_use]
    pub fn quote(&self, quote: QuoteIdx) -> Option<&Quote> {
        self.quotes.get(quote)
    }

    /// The handle naming whatever occupies quote slot `index`, for walking an intrusive leg
    /// chain and recovering the generation-carrying handle it names (§3).
    #[must_use]
    pub fn quote_handle_at(&self, index: u32) -> Option<QuoteIdx> {
        self.quotes.handle_at(index)
    }

    /// Live quotes. Fixed capacity; the allocation proxy of CLAUDE 25.
    #[must_use]
    pub fn quote_count(&self) -> u32 {
        self.quotes.len()
    }

    /// Every live quote, in dense index order.
    ///
    /// A read model, not a query surface: §16 notes the engine can answer "what is my
    /// capital locked against" and that nothing *exposes* it. This is the accessor the
    /// scenario runner traces through and the tests assert over; no command reaches it, and
    /// nothing on a mutating path calls it.
    pub fn quotes(&self) -> impl Iterator<Item = (QuoteIdx, &Quote)> {
        self.quotes.iter()
    }

    /// Every live request, in dense index order.
    pub fn requests(&self) -> impl Iterator<Item = (ReqIdx, &Request)> {
        self.requests.iter()
    }

    /// Every live claim, in dense index order.
    pub fn reservations(&self) -> impl Iterator<Item = (ResIdx, &Reservation)> {
        self.reservations.iter()
    }

    /// The handle naming whatever occupies request slot `index`.
    ///
    /// How a nonce becomes a request again (§8.1): the caller compares the generation it
    /// carries with the one this returns, and a mismatch means the slot has been reused and
    /// the nonce names nothing. No handle is constructed from outside — this hands back the
    /// one the slab already holds.
    #[must_use]
    pub fn request_handle_at(&self, index: u32) -> Option<ReqIdx> {
        self.requests.handle_at(index)
    }

    /// Live requests.
    #[must_use]
    pub fn request_count(&self) -> u32 {
        self.requests.len()
    }

    /// A claim, if the handle resolves. `None` means the handle is stale (§15.3).
    #[must_use]
    pub fn reservation(&self, claim: ResIdx) -> Option<&Reservation> {
        self.reservations.get(claim)
    }

    /// Live claims. Fixed capacity; this is the allocation proxy of CLAUDE 25.
    #[must_use]
    pub fn reservation_count(&self) -> u32 {
        self.reservations.len()
    }

    /// Claim-slab capacity. Never changes after construction.
    #[must_use]
    pub fn reservation_capacity(&self) -> u32 {
        self.reservations.capacity()
    }

    // ─────────────────────────── moving a claim (§4.3) ───────────────────────────

    /// Claim `amount` of `account`'s balance until `expires_at`, on behalf of `owner`.
    ///
    /// Plan, check, then commit: every fallible step happens before the first mutation, so
    /// a rejection leaves the ledger byte-identical (§15.4, CLAUDE 18).
    ///
    /// # Errors
    ///
    /// [`LedgerError::UnknownAccount`], [`LedgerError::StaleOwner`],
    /// [`LedgerError::OwnerAlreadyClaimed`], [`LedgerError::AmountOverflow`],
    /// [`LedgerError::InsufficientFree`], [`LedgerError::SlabExhausted`].
    pub fn reserve(
        &mut self,
        account: AccountIdx,
        amount: Amount,
        expires_at: Ts,
        owner: ResOwner,
    ) -> Result<ResIdx, LedgerError> {
        // ── PLAN / CHECK ──
        let held = match owner {
            ResOwner::Quote(quote) => self.quotes.get(quote).ok_or(LedgerError::StaleOwner)?.claim(),
            ResOwner::Request(request) => {
                self.requests.get(request).ok_or(LedgerError::StaleOwner)?.claim()
            }
        };
        if held.is_some() {
            return Err(LedgerError::OwnerAlreadyClaimed);
        }
        let reserved = self.check_reservable(account, amount)?;

        // Last CHECK-phase step. It is fallible and it takes a slot, but a *failed* insert
        // mutates nothing — the slab does not grow, it refuses (CLAUDE 10) — so a rejection
        // here still leaves the ledger byte-identical. The commit boundary is drawn below it
        // because that is where infallibility actually begins.
        let claim = self
            .reservations
            .insert(Reservation::reserved(account, amount, owner, expires_at))
            .map_err(|_| LedgerError::SlabExhausted { slab: SlabKind::Reservation })?;

        // ── COMMIT ──
        self.link_claim(account, claim, expires_at, owner, reserved);
        self.assert_invariants();
        Ok(claim)
    }

    /// Open a quote and reserve against it in one command.
    ///
    /// Two slabs and a balance move together or not at all. Doing it as two ledger calls
    /// would leave a quote slot taken when the reservation is refused, which is a partially
    /// applied command (CLAUDE 19) — so both slabs are checked for room *before* either
    /// insert, and neither insert can then fail.
    ///
    /// This is the shape S2's `SubmitQuote` needs; S2 adds price, size and the per-leg
    /// chain on top of it.
    ///
    /// # Errors
    ///
    /// [`LedgerError::UnknownAccount`], [`LedgerError::InsufficientFree`],
    /// [`LedgerError::AmountOverflow`], [`LedgerError::SlabExhausted`].
    pub fn open_quote_reserving(
        &mut self,
        record: Quote,
        amount: Amount,
    ) -> Result<(QuoteIdx, ResIdx), LedgerError> {
        // ── PLAN / CHECK ──
        // The claim expires exactly when the quote does — one value, not two that could
        // drift apart.
        let expires_at = record.expires_at();
        let account = record.maker();
        let reserved = self.check_reservable(account, amount)?;
        if !self.quotes.has_room() {
            return Err(LedgerError::SlabExhausted { slab: SlabKind::Quote });
        }
        if !self.reservations.has_room() {
            return Err(LedgerError::SlabExhausted { slab: SlabKind::Reservation });
        }

        // Both slabs were just shown to have room and nothing between here and the inserts
        // consumes a slot, so neither `?` can fire. They are written fallibly because
        // `insert` is; a failure would mean `has_room` lied, which the slab's own debug
        // assertions catch.
        let quote = self
            .quotes
            .insert(record)
            .map_err(|_| LedgerError::SlabExhausted { slab: SlabKind::Quote })?;
        let owner = ResOwner::Quote(quote);
        let claim = self
            .reservations
            .insert(Reservation::reserved(account, amount, owner, expires_at))
            .map_err(|_| LedgerError::SlabExhausted { slab: SlabKind::Reservation })?;

        // ── COMMIT ──
        self.link_claim(account, claim, expires_at, owner, reserved);
        self.assert_invariants();
        Ok((quote, claim))
    }

    /// Open a request and reserve the requester's claim against it in one command.
    ///
    /// The requester's `Σ size × limit_price` is reserved at `SubmitRequest`, before any
    /// price exists (§5.2), which is why this is one operation and not two.
    ///
    /// # Errors
    ///
    /// As [`Ledger::open_quote_reserving`].
    pub fn open_request_reserving(
        &mut self,
        record: Request,
        amount: Amount,
    ) -> Result<(ReqIdx, ResIdx), LedgerError> {
        // ── PLAN / CHECK ──
        // The requester's claim expires with the request: past the deadline the request is
        // `Expired` (derived) and the capital is reclaimed by release-on-access (§5).
        let expires_at = record.deadline();
        let account = record.requester();
        let reserved = self.check_reservable(account, amount)?;
        if !self.requests.has_room() {
            return Err(LedgerError::SlabExhausted { slab: SlabKind::Request });
        }
        if !self.reservations.has_room() {
            return Err(LedgerError::SlabExhausted { slab: SlabKind::Reservation });
        }

        let request = self
            .requests
            .insert(record)
            .map_err(|_| LedgerError::SlabExhausted { slab: SlabKind::Request })?;
        let owner = ResOwner::Request(request);
        let claim = self
            .reservations
            .insert(Reservation::reserved(account, amount, owner, expires_at))
            .map_err(|_| LedgerError::SlabExhausted { slab: SlabKind::Reservation })?;

        // ── COMMIT ──
        self.link_claim(account, claim, expires_at, owner, reserved);
        self.assert_invariants();
        Ok((request, claim))
    }

    /// Claim coverage (§15.6), checked against the mirror before the claim exists rather
    /// than asserted after it. Reserving moves no money — `free` is untouched — so the
    /// question is whether the claims *together* still fit inside the mirrored balance.
    ///
    /// Returns what `account.reserved` becomes if the claim is admitted.
    fn check_reservable(
        &self,
        account: AccountIdx,
        amount: Amount,
    ) -> Result<Amount, LedgerError> {
        let entry = self.accounts.get(account.0 as usize).ok_or(LedgerError::UnknownAccount)?;
        let claimed = entry
            .reserved()
            .checked_add(entry.committed())
            .and_then(|total| total.checked_add(amount))
            .ok_or(LedgerError::AmountOverflow)?;
        if claimed > entry.free() {
            return Err(LedgerError::InsufficientFree);
        }
        entry.reserved().checked_add(amount).ok_or(LedgerError::AmountOverflow)
    }

    /// The infallible tail every reservation path shares.
    fn link_claim(
        &mut self,
        account: AccountIdx,
        claim: ResIdx,
        expires_at: Ts,
        owner: ResOwner,
        reserved: Amount,
    ) {
        self.link_into_expiry_chain(account, claim.index(), expires_at);
        if let Some(entry) = self.accounts.get_mut(account.0 as usize) {
            entry.set_reserved(reserved);
        }
        self.set_owner_claim(owner, Some(claim));
    }

    /// Release a named claim, returning the amount freed.
    ///
    /// Named claims are validated; bulk reclamation is not, and does not need to be
    /// (§4.3).
    ///
    /// # Errors
    ///
    /// [`LedgerError::StaleReservation`] if the handle's generation no longer matches;
    /// [`LedgerError::ReservationCommitted`] if it names committed capital, which may not
    /// be released on a guess (§8.3). Two different bugs, two different variants.
    pub fn release(&mut self, claim: ResIdx) -> Result<Amount, LedgerError> {
        // ── PLAN / CHECK ──
        let reservation = self.reservations.get(claim).ok_or(LedgerError::StaleReservation)?;
        if reservation.is_committed() {
            return Err(LedgerError::ReservationCommitted);
        }
        let account = reservation.account();
        let amount = reservation.amount();
        let owner = reservation.owner();
        let entry = self.accounts.get(account.0 as usize).ok_or(LedgerError::UnknownAccount)?;
        let reserved = entry.reserved().checked_sub(amount).ok_or(LedgerError::AmountOverflow)?;

        // ── COMMIT ──
        self.unlink_from_chain(account, claim.index());
        if let Some(entry) = self.accounts.get_mut(account.0 as usize) {
            entry.set_reserved(reserved);
        }
        self.set_owner_claim(owner, None);
        let removed = self.reservations.remove(claim);
        debug_assert!(removed.is_some(), "a claim that resolved a moment ago must remove");

        self.assert_invariants();
        Ok(amount)
    }

    /// Move a claim `reserved → committed`: unlink from the account's expiry chain and link
    /// into `request`'s committed list, in one step (§4.3, §7.2).
    ///
    /// After this the claim has no expiry, so `release_expired` cannot reach it — not
    /// because it is filtered out, but because it is no longer on the chain that traversal
    /// walks and no longer has the field that traversal reads.
    ///
    /// # Errors
    ///
    /// [`LedgerError::StaleReservation`], [`LedgerError::ReservationCommitted`] if it is
    /// already committed, [`LedgerError::StaleRequest`], [`LedgerError::AmountOverflow`].
    pub fn commit(&mut self, claim: ResIdx, request: ReqIdx) -> Result<(), LedgerError> {
        // ── PLAN / CHECK ──
        let reservation = self.reservations.get(claim).ok_or(LedgerError::StaleReservation)?;
        if reservation.is_committed() {
            return Err(LedgerError::ReservationCommitted);
        }
        let account = reservation.account();
        let amount = reservation.amount();
        if self.requests.get(request).is_none() {
            return Err(LedgerError::StaleRequest);
        }
        let entry = self.accounts.get(account.0 as usize).ok_or(LedgerError::UnknownAccount)?;
        // Both totals are computed before anything moves, so the step below is infallible.
        let reserved = entry.reserved().checked_sub(amount).ok_or(LedgerError::AmountOverflow)?;
        let committed =
            entry.committed().checked_add(amount).ok_or(LedgerError::AmountOverflow)?;

        // ── COMMIT ──
        self.unlink_from_chain(account, claim.index());
        self.link_into_committed_list(request, claim.index());
        if let Some(entry) = self.accounts.get_mut(account.0 as usize) {
            entry.set_reserved(reserved);
            entry.set_committed(committed);
        }

        self.assert_invariants();
        Ok(())
    }

    /// Reclaim every expired claim on `account`, from the head of its expiry chain,
    /// stopping at the first live entry. Returns how many were reclaimed.
    ///
    /// This is normalisation (§4.3) and the sole reclamation mechanism: there is no sweeper
    /// and no global expiry structure, so correctness never depends on a background pass
    /// having run.
    ///
    /// **It emits no events.** The count is bounded only by how many expired, so emitting
    /// per reclamation would put an unbounded write into the event buffer before any CHECK
    /// phase could verify headroom (CLAUDE 9). Expiry is a derived fact — a maker's own
    /// liveness predicate tells them their quote is dead — so no notification is owed.
    ///
    /// It also has **no committed-capital guard**, and needs none: committed claims are not
    /// on this chain. If one were, the ledger is already corrupt, and the `debug_assert`
    /// below says so rather than quietly skipping it.
    ///
    /// # Errors
    ///
    /// [`LedgerError::UnknownAccount`].
    pub fn release_expired(
        &mut self,
        account: AccountIdx,
        now: Ts,
    ) -> Result<u32, LedgerError> {
        let entry = self.accounts.get(account.0 as usize).ok_or(LedgerError::UnknownAccount)?;
        let mut cursor = entry.expiry_head;
        let mut reclaimed: u32 = 0;

        while let Some(index) = cursor.index() {
            let Some(reservation) = self.reservation_at(index) else {
                debug_assert!(false, "expiry chain names a vacant slot");
                break;
            };
            let Some(expired) = reservation.is_expired_at(now) else {
                // A committed claim on the expiry chain. Not filtered — reported: the two
                // chains are disjoint by construction and this cannot happen on an intact
                // ledger (§4.3).
                debug_assert!(false, "committed claim found on an expiry chain");
                break;
            };
            if !expired {
                // Half-open liveness: live iff `now < expires_at` (§4.2). The chain is
                // ordered, so the first live entry ends the walk.
                break;
            }

            let amount = reservation.amount();
            let owner = reservation.owner();
            let (_, next) = reservation.neighbours();
            let Some(handle) = self.reservations.handle_at(index) else {
                debug_assert!(false, "an occupied slot must yield a handle");
                break;
            };

            self.unlink_from_chain(account, index);
            if let Some(entry) = self.accounts.get_mut(account.0 as usize) {
                let reserved = Amount(entry.reserved().0.saturating_sub(amount.0));
                entry.set_reserved(reserved);
            }
            self.set_owner_claim(owner, None);
            let removed = self.reservations.remove(handle);
            debug_assert!(removed.is_some(), "an occupied slot must remove");

            reclaimed = reclaimed.saturating_add(1);
            cursor = next;
        }

        // §15.2, scoped to the account just normalised — never the global expiry-predicate
        // form, which is unsatisfiable against a stored total for any untouched account.
        self.assert_normalised(account, now);
        self.assert_invariants();
        Ok(reclaimed)
    }

    // ────────────────────────────── chain plumbing ──────────────────────────────

    fn reservation_at(&self, index: u32) -> Option<&Reservation> {
        let handle = self.reservations.handle_at(index)?;
        self.reservations.get(handle)
    }

    fn reservation_at_mut(&mut self, index: u32) -> Option<&mut Reservation> {
        let handle = self.reservations.handle_at(index)?;
        self.reservations.get_mut(handle)
    }

    fn set_owner_claim(&mut self, owner: ResOwner, claim: Option<ResIdx>) {
        match owner {
            ResOwner::Quote(quote) => match self.quotes.get_mut(quote) {
                Some(quote) => quote.set_claim(claim),
                None => debug_assert!(false, "a claim's owner must resolve (§15.3)"),
            },
            ResOwner::Request(request) => match self.requests.get_mut(request) {
                Some(request) => request.set_claim(claim),
                None => debug_assert!(false, "a claim's owner must resolve (§15.3)"),
            },
        }
    }

    /// Insert into the account's expiry chain, **walking back from the tail**.
    ///
    /// Near-O(1): a maker's quotes usually expire later than the ones already standing, so
    /// the walk stops immediately. Worst case is O(k) over one account's chain, and `k` is
    /// bounded in practice because every live claim locks real capital — an account cannot
    /// hold more open quotes than its balance supports (§4.3).
    fn link_into_expiry_chain(&mut self, account: AccountIdx, index: u32, expires_at: Ts) {
        let Some(entry) = self.accounts.get(account.0 as usize) else {
            debug_assert!(false, "linking a claim into an unknown account");
            return;
        };

        let mut cursor = entry.expiry_tail;
        let after = loop {
            let Some(candidate) = cursor.index() else { break None };
            let Some(reservation) = self.reservation_at(candidate) else {
                debug_assert!(false, "expiry chain names a vacant slot");
                break None;
            };
            let ClaimLinks::Reserved { expires_at: theirs, prev, .. } = reservation.links()
            else {
                debug_assert!(false, "committed claim found on an expiry chain");
                break None;
            };
            // Ascending, with equal expiries ordered by arrival: stop at the first entry
            // that does not expire later, and insert after it.
            if theirs <= expires_at {
                break Some(candidate);
            }
            cursor = prev;
        };

        if let Some(after) = after {
            self.splice_after(account, index, after);
        } else {
            self.splice_at_head(account, index);
        }
    }

    /// Splice `index` in immediately after `after`, fixing the tail if `after` was it.
    fn splice_after(&mut self, account: AccountIdx, index: u32, after: u32) {
        let next =
            self.reservation_at(after).map_or(Link::NIL, |reservation| reservation.neighbours().1);
        if let Some(reservation) = self.reservation_at_mut(after) {
            reservation.set_next(Link::to(index));
        }
        if let Some(reservation) = self.reservation_at_mut(index) {
            reservation.set_prev(Link::to(after));
            reservation.set_next(next);
        }
        if let Some(next) = next.index() {
            if let Some(reservation) = self.reservation_at_mut(next) {
                reservation.set_prev(Link::to(index));
            }
        } else if let Some(entry) = self.accounts.get_mut(account.0 as usize) {
            entry.expiry_tail = Link::to(index);
        }
    }

    /// Splice `index` in at the head, fixing the tail if the chain was empty.
    fn splice_at_head(&mut self, account: AccountIdx, index: u32) {
        let head = self.accounts.get(account.0 as usize).map_or(Link::NIL, |entry| entry.expiry_head);
        if let Some(reservation) = self.reservation_at_mut(index) {
            reservation.set_prev(Link::NIL);
            reservation.set_next(head);
        }
        if let Some(head) = head.index() {
            if let Some(reservation) = self.reservation_at_mut(head) {
                reservation.set_prev(Link::to(index));
            }
        } else if let Some(entry) = self.accounts.get_mut(account.0 as usize) {
            entry.expiry_tail = Link::to(index);
        }
        if let Some(entry) = self.accounts.get_mut(account.0 as usize) {
            entry.expiry_head = Link::to(index);
        }
    }

    /// Unlink from the account's expiry chain. O(1), and it fixes **both** endpoints —
    /// removing the last entry must clear the tail, not just the head.
    fn unlink_from_chain(&mut self, account: AccountIdx, index: u32) {
        let Some((prev, next)) = self.reservation_at(index).map(Reservation::neighbours) else {
            debug_assert!(false, "unlinking a claim that is not in the slab");
            return;
        };

        match prev.index() {
            Some(prev_index) => {
                if let Some(reservation) = self.reservation_at_mut(prev_index) {
                    reservation.set_next(next);
                }
            }
            None => {
                if let Some(entry) = self.accounts.get_mut(account.0 as usize) {
                    entry.expiry_head = next;
                }
            }
        }
        match next.index() {
            Some(next_index) => {
                if let Some(reservation) = self.reservation_at_mut(next_index) {
                    reservation.set_prev(prev);
                }
            }
            None => {
                if let Some(entry) = self.accounts.get_mut(account.0 as usize) {
                    entry.expiry_tail = prev;
                }
            }
        }

        if let Some(reservation) = self.reservation_at_mut(index) {
            reservation.set_prev(Link::NIL);
            reservation.set_next(Link::NIL);
        }
    }

    /// Link into the request's committed list. Head insertion, O(1): the list is unordered,
    /// because nothing ever asks it for an earliest element.
    fn link_into_committed_list(&mut self, request: ReqIdx, index: u32) {
        let head = self.requests.get(request).map_or(Link::NIL, |request| request.committed_head);

        if let Some(reservation) = self.reservation_at_mut(index) {
            reservation.set_links(ClaimLinks::Committed {
                request,
                prev: Link::NIL,
                next: head,
            });
        }
        if let Some(head_index) = head.index()
            && let Some(reservation) = self.reservation_at_mut(head_index)
        {
            reservation.set_prev(Link::to(index));
        }
        if let Some(request) = self.requests.get_mut(request) {
            request.committed_head = Link::to(index);
        }
    }
}

// ─────────────────────────── the commit phase (SPEC §7.2) ───────────────────────────

impl Ledger {
    /// Enter the commit phase.
    ///
    /// [`CommitPhase`] exposes **only infallible operations**, so CLAUDE 18's "no `?`, no
    /// fallible call past this line" is enforced by the type rather than by review. Every
    /// method on it documents the precondition the CHECK phase must already have
    /// established.
    pub const fn commit_phase(&mut self) -> CommitPhase<'_> {
        CommitPhase { ledger: self }
    }

    /// A quote, mutably. Infallible in shape — the `Option` is a branch, not a `?`.
    pub(crate) fn quote_mut(&mut self, quote: QuoteIdx) -> Option<&mut Quote> {
        self.quotes.get_mut(quote)
    }

    /// A request, mutably.
    pub(crate) fn request_mut(&mut self, request: ReqIdx) -> Option<&mut Request> {
        self.requests.get_mut(request)
    }
}

/// The commit phase of a command: infallible bookkeeping, and nothing else.
///
/// No method here returns a `Result`, so no `?` can appear and no early return can leave a
/// command half applied. Where a precondition could in principle fail — a handle that no
/// longer resolves, a claim already committed — the method `debug_assert`s and does nothing,
/// because on a ledger whose CHECK phase has run those states are unreachable, and a partial
/// mutation would be worse than a missed one.
#[derive(Debug)]
pub struct CommitPhase<'a> {
    ledger: &'a mut Ledger,
}

impl CommitPhase<'_> {
    /// Release a reserved claim and free its slot.
    ///
    /// Precondition, established in PLAN: `claim` resolves and is `reserved`.
    pub fn release(&mut self, claim: ResIdx) {
        let Some(reservation) = self.ledger.reservations.get(claim) else {
            debug_assert!(false, "the commit phase released a claim that does not resolve");
            return;
        };
        if reservation.is_committed() {
            debug_assert!(false, "the commit phase released committed capital (SPEC §8.3)");
            return;
        }
        let account = reservation.account();
        let amount = reservation.amount();
        let owner = reservation.owner();

        self.ledger.unlink_from_chain(account, claim.index());
        if let Some(entry) = self.ledger.accounts.get_mut(account.0 as usize) {
            let reserved = Amount(entry.reserved().0.saturating_sub(amount.0));
            entry.set_reserved(reserved);
        }
        self.ledger.set_owner_claim(owner, None);
        self.ledger.reservations.remove(claim);
        self.ledger.assert_invariants();
    }

    /// Move `keep` of a reserved claim to `committed` on `request`'s list, returning the
    /// remainder to `free`.
    ///
    /// This is both halves of §7.2's commit step. The requester's claim is reserved at
    /// `Σ size × limit` before any price exists, and fills at `Σ size × fill`, so the
    /// over-reservation is released here rather than by a second command — one claim, one
    /// transition, no window in which the difference belongs to neither bucket.
    ///
    /// Precondition, established in CHECK: `claim` resolves, is `reserved`, and
    /// `keep <= claim.amount`.
    pub fn commit(&mut self, claim: ResIdx, request: ReqIdx, keep: Amount) {
        let Some(reservation) = self.ledger.reservations.get(claim) else {
            debug_assert!(false, "the commit phase committed a claim that does not resolve");
            return;
        };
        if reservation.is_committed() {
            debug_assert!(false, "the commit phase committed an already-committed claim");
            return;
        }
        debug_assert!(keep <= reservation.amount(), "CHECK must bound `keep` by the claim");
        let account = reservation.account();
        let reserved_before = reservation.amount();
        let keep = Amount(keep.0.min(reserved_before.0));

        self.ledger.unlink_from_chain(account, claim.index());
        if let Some(reservation) = self.ledger.reservation_at_mut(claim.index()) {
            reservation.set_amount(keep);
        }
        self.ledger.link_into_committed_list(request, claim.index());
        if let Some(entry) = self.ledger.accounts.get_mut(account.0 as usize) {
            let reserved = Amount(entry.reserved().0.saturating_sub(reserved_before.0));
            let committed = Amount(entry.committed().0.saturating_add(keep.0));
            entry.set_reserved(reserved);
            entry.set_committed(committed);
        }
        self.ledger.assert_invariants();
    }

    /// Free a quote slot whose claim has already been released.
    ///
    /// Precondition: `quote` holds no claim.
    pub fn close_quote(&mut self, quote: QuoteIdx) {
        debug_assert!(
            self.ledger.quotes.get(quote).is_some_and(|quote| quote.claim().is_none()),
            "the commit phase freed an owner that still holds capital (SPEC §15.3)"
        );
        self.ledger.quotes.remove(quote);
        self.ledger.assert_invariants();
    }

    /// Discharge every claim on a request's committed list, returning them to the accounts'
    /// free capital and freeing their slots. Calls `visit` with each claim's owner and
    /// amount as it goes.
    ///
    /// One operation for both settlement outcomes, and deliberately. From the engine's side
    /// `committed → escrowed` and `committed → free` are the same bookkeeping: the claim
    /// leaves the core's books. Which of the two happened is a fact about custody, recorded
    /// in the request's state, not a different ledger move — and inventing a second one
    /// would put the engine in the position of tracking escrows, which are custody's (§2.4,
    /// §13.1).
    ///
    /// Precondition, established in CHECK: the request resolves and its status is final.
    pub fn discharge_committed<F: FnMut(ResOwner, Amount)>(
        &mut self,
        request: ReqIdx,
        mut visit: F,
    ) {
        let mut cursor =
            self.ledger.requests.get(request).map_or(Link::NIL, |record| record.committed_head);
        let mut steps: u32 = 0;
        while let Some(index) = cursor.index() {
            let Some(reservation) = self.ledger.reservation_at(index) else {
                debug_assert!(false, "a committed list names a vacant slot");
                break;
            };
            let account = reservation.account();
            let amount = reservation.amount();
            let owner = reservation.owner();
            let next = reservation.neighbours().1;
            let Some(handle) = self.ledger.reservations.handle_at(index) else { break };

            if let Some(entry) = self.ledger.accounts.get_mut(account.0 as usize) {
                let committed = Amount(entry.committed().0.saturating_sub(amount.0));
                entry.set_committed(committed);
            }
            self.ledger.set_owner_claim(owner, None);
            self.ledger.reservations.remove(handle);
            visit(owner, amount);

            cursor = next;
            steps = steps.saturating_add(1);
            if steps > self.ledger.reservations.capacity() {
                debug_assert!(false, "a committed list is cyclic");
                break;
            }
        }
        if let Some(record) = self.ledger.requests.get_mut(request) {
            record.committed_head = Link::NIL;
            record.set_claim(None);
        }
        self.ledger.assert_invariants();
    }

    /// A quote, mutably. Field writes only; nothing here can fail.
    pub fn quote_mut(&mut self, quote: QuoteIdx) -> Option<&mut Quote> {
        self.ledger.quote_mut(quote)
    }

    /// A request, mutably.
    pub fn request_mut(&mut self, request: ReqIdx) -> Option<&mut Request> {
        self.ledger.request_mut(request)
    }
}

// ─────────────────────────── the invariants (SPEC §15.1–§15.3) ───────────────────────────

impl Ledger {
    /// Every structural invariant the ledger is responsible for, in the **chain-sum** form.
    ///
    /// Chain-sum, never the expiry-predicate form: `account.reserved` is a stored value,
    /// while a predicate over `expires_at` shrinks with the passage of time alone. Asserting
    /// `reserved == Σ` over *live* claims would be unsatisfiable for any un-normalised
    /// account, and the pressure would then be to weaken the assertion rather than fix the
    /// ledger (CLAUDE 16, §15.1 vs §15.2).
    ///
    /// Returned rather than panicked so a test can name the violation; the ledger asserts it
    /// after every mutation via `debug_assert`.
    ///
    /// Cost is O(accounts × committed claims) and it runs only in debug builds. The
    /// alternative — accumulating per-account sums in one pass — needs a scratch buffer,
    /// and no container in the engine may grow (CLAUDE 9).
    ///
    /// # Errors
    ///
    /// The first [`InvariantViolation`] found.
    pub fn check_invariants(&self) -> Result<(), InvariantViolation> {
        self.check_structural_invariants()?;
        self.check_claim_state_coherence()
    }

    /// The half of §15 that holds after **every mutation**: chain sums, chain shape, and the
    /// bidirectional owner link.
    ///
    /// # Errors
    ///
    /// The first [`InvariantViolation`] found.
    pub fn check_structural_invariants(&self) -> Result<(), InvariantViolation> {
        let mut chained: u32 = 0;
        chained = chained.saturating_add(self.check_expiry_chains()?);
        chained = chained.saturating_add(self.check_committed_lists()?);
        self.check_committed_totals()?;

        // Every live claim is on exactly one chain. A claim on none, or on both, changes
        // this count — which is how a `commit` that forgot to unlink is caught even before
        // the sums disagree.
        if chained != self.reservations.len() {
            return Err(InvariantViolation::ClaimNotOnExactlyOneChain);
        }

        self.check_owners()
    }

    /// The half of §15.3 that holds after every **command**, not after every mutation: a
    /// `reserved` claim references a standing quote or an open request, and a `committed`
    /// claim references exactly one `Consumed` quote on a request in `Settling`, or that
    /// request itself.
    ///
    /// Scoped to the command rather than the step, deliberately. A commit phase that moves
    /// several claims has intermediate states by construction — the first winning maker's
    /// claim is `committed` before the request has been marked `Settling`, because both
    /// cannot happen in the same instruction. §15.4 already establishes that the observable
    /// unit is the command, and asserting a whole-command property after every field write
    /// would be asserting something the design never claimed.
    ///
    /// # Errors
    ///
    /// The first [`InvariantViolation`] found.
    pub fn check_claim_state_coherence(&self) -> Result<(), InvariantViolation> {
        for (claim, reservation) in self.reservations.iter() {
            match (reservation.committed_to(), reservation.owner()) {
                // A reserved maker claim backs a standing quote.
                (None, ResOwner::Quote(quote)) => {
                    let record =
                        self.quotes.get(quote).ok_or(InvariantViolation::OwnerDoesNotResolve(claim))?;
                    if !matches!(record.state(), QuoteState::Active) {
                        return Err(InvariantViolation::ReservedClaimOwnerNotStanding(claim));
                    }
                }
                // A reserved requester claim backs an open request.
                (None, ResOwner::Request(request)) => {
                    let record = self
                        .requests
                        .get(request)
                        .ok_or(InvariantViolation::OwnerDoesNotResolve(claim))?;
                    if !matches!(record.state(), RequestState::Open) {
                        return Err(InvariantViolation::ReservedClaimOwnerNotStanding(claim));
                    }
                }
                // A committed maker claim backs a Consumed quote on the settling request.
                (Some(list), ResOwner::Quote(quote)) => {
                    let record =
                        self.quotes.get(quote).ok_or(InvariantViolation::OwnerDoesNotResolve(claim))?;
                    if !matches!(record.state(), QuoteState::Consumed) {
                        return Err(InvariantViolation::CommittedClaimOwnerNotConsumed(claim));
                    }
                    if record.request() != list {
                        return Err(InvariantViolation::ClaimOnForeignCommittedList(list));
                    }
                    self.require_settling(list, claim)?;
                }
                // A committed requester claim is on its own request's list.
                (Some(list), ResOwner::Request(request)) => {
                    if request != list {
                        return Err(InvariantViolation::ClaimOnForeignCommittedList(list));
                    }
                    self.require_settling(list, claim)?;
                }
            }
        }
        Ok(())
    }

    fn require_settling(
        &self,
        request: ReqIdx,
        claim: ResIdx,
    ) -> Result<(), InvariantViolation> {
        let record =
            self.requests.get(request).ok_or(InvariantViolation::OwnerDoesNotResolve(claim))?;
        if matches!(record.state(), RequestState::Open) {
            return Err(InvariantViolation::CommittedClaimRequestNotSettling(claim));
        }
        Ok(())
    }

    /// §15.1, reserved half: every expiry chain is well-formed, ordered, and sums to its
    /// account's stored `reserved`. Returns how many claims were on the chains.
    fn check_expiry_chains(&self) -> Result<u32, InvariantViolation> {
        let budget = self.reservations.capacity();
        let mut chained: u32 = 0;

        for (index, entry) in self.accounts.iter().enumerate() {
            let account = AccountIdx(u32::try_from(index).unwrap_or(u32::MAX));
            let mut sum = Amount::ZERO;
            let mut previous = Link::NIL;
            let mut cursor = entry.expiry_head;
            let mut last_expiry: Option<Ts> = None;

            while let Some(at) = cursor.index() {
                let Some(reservation) = self.reservation_at(at) else {
                    return Err(InvariantViolation::ChainLinksInconsistent(account));
                };
                let ClaimLinks::Reserved { expires_at, prev, next } = reservation.links() else {
                    return Err(InvariantViolation::CommittedClaimOnExpiryChain(account));
                };
                if reservation.account() != account {
                    return Err(InvariantViolation::ClaimOnForeignChain(account));
                }
                if prev != previous {
                    return Err(InvariantViolation::ChainLinksInconsistent(account));
                }
                if last_expiry.is_some_and(|last| expires_at < last) {
                    return Err(InvariantViolation::ExpiryChainOutOfOrder(account));
                }
                sum = sum
                    .checked_add(reservation.amount())
                    .ok_or(InvariantViolation::ReservedTotalMismatch(account))?;

                last_expiry = Some(expires_at);
                previous = cursor;
                cursor = next;
                chained = chained.saturating_add(1);
                if chained > budget {
                    // A cycle. Bounded rather than trusted, so a broken unlink surfaces as
                    // a failed assertion and not as a hang.
                    return Err(InvariantViolation::ChainLinksInconsistent(account));
                }
            }

            if previous != entry.expiry_tail {
                return Err(InvariantViolation::ChainLinksInconsistent(account));
            }
            if sum != entry.reserved() {
                return Err(InvariantViolation::ReservedTotalMismatch(account));
            }
        }

        Ok(chained)
    }

    /// Every committed list is well-formed and holds only committed claims naming it.
    /// Returns how many claims were on the lists.
    fn check_committed_lists(&self) -> Result<u32, InvariantViolation> {
        let budget = self.reservations.capacity();
        let mut chained: u32 = 0;

        for (request_handle, request) in self.requests.iter() {
            let mut previous = Link::NIL;
            let mut cursor = request.committed_head;
            while let Some(at) = cursor.index() {
                let Some(reservation) = self.reservation_at(at) else {
                    return Err(InvariantViolation::ClaimOnForeignCommittedList(request_handle));
                };
                let ClaimLinks::Committed { request: owner_request, prev, next } =
                    reservation.links()
                else {
                    return Err(InvariantViolation::ReservedClaimOnCommittedList(request_handle));
                };
                if owner_request != request_handle || prev != previous {
                    return Err(InvariantViolation::ClaimOnForeignCommittedList(request_handle));
                }
                previous = cursor;
                cursor = next;
                chained = chained.saturating_add(1);
                if chained > budget {
                    return Err(InvariantViolation::ClaimOnForeignCommittedList(request_handle));
                }
            }
        }

        Ok(chained)
    }

    /// §15.1, committed half: `account.committed == Σ` that account's amounts across the
    /// committed lists.
    fn check_committed_totals(&self) -> Result<(), InvariantViolation> {
        for (index, entry) in self.accounts.iter().enumerate() {
            let account = AccountIdx(u32::try_from(index).unwrap_or(u32::MAX));
            let mut sum = Amount::ZERO;
            for (_, request) in self.requests.iter() {
                let mut cursor = request.committed_head;
                while let Some(at) = cursor.index() {
                    let Some(reservation) = self.reservation_at(at) else { break };
                    if reservation.account() == account {
                        sum = sum
                            .checked_add(reservation.amount())
                            .ok_or(InvariantViolation::CommittedTotalMismatch(account))?;
                    }
                    cursor = reservation.neighbours().1;
                }
            }
            if sum != entry.committed() {
                return Err(InvariantViolation::CommittedTotalMismatch(account));
            }
        }
        Ok(())
    }

    /// §15.3. The owner **must resolve**, and must point back. Treating a failed
    /// dereference as "nothing to check" would blind this to the one case it exists for:
    /// a claim naming a slot that has been freed and reissued (CLAUDE 42).
    fn check_owners(&self) -> Result<(), InvariantViolation> {
        for (claim, reservation) in self.reservations.iter() {
            let held = match reservation.owner() {
                ResOwner::Quote(quote) => self
                    .quotes
                    .get(quote)
                    .ok_or(InvariantViolation::OwnerDoesNotResolve(claim))?
                    .claim(),
                ResOwner::Request(request) => self
                    .requests
                    .get(request)
                    .ok_or(InvariantViolation::OwnerDoesNotResolve(claim))?
                    .claim(),
            };
            if held != Some(claim) {
                return Err(InvariantViolation::OwnerDoesNotPointBack(claim));
            }
        }

        // And the same relationship read from the owner's end. Checking only claim → owner
        // would miss an owner left pointing at a claim that has been released: the freed
        // claim is no longer iterated, so the first direction has nothing to look at. A
        // bidirectional invariant has to be asserted in both directions or it is half an
        // invariant.
        for (handle, quote) in self.quotes.iter() {
            if let Some(claim) = quote.claim() {
                let reservation = self
                    .reservations
                    .get(claim)
                    .ok_or(InvariantViolation::OwnerDoesNotPointBack(claim))?;
                if reservation.owner() != ResOwner::Quote(handle) {
                    return Err(InvariantViolation::OwnerDoesNotPointBack(claim));
                }
            }
        }
        for (handle, request) in self.requests.iter() {
            if let Some(claim) = request.claim() {
                let reservation = self
                    .reservations
                    .get(claim)
                    .ok_or(InvariantViolation::OwnerDoesNotPointBack(claim))?;
                if reservation.owner() != ResOwner::Request(handle) {
                    return Err(InvariantViolation::OwnerDoesNotPointBack(claim));
                }
            }
        }

        Ok(())
    }

    /// §15.6 for **one account**, which is the only form worth evaluating per command.
    ///
    /// A command addresses at most two accounts ([`Engine::touched_accounts`]), so scanning
    /// the whole preallocated table to check them is work proportional to the venue rather
    /// than to the command. `check_normalised` already scopes itself for the same reason and
    /// says so; this is that argument applied to the other half.
    ///
    /// **This predicate is not unconditional, and the caller must know when it holds.** It
    /// compares claims against the *mirror*, which projects custody's **availability** —
    /// balance minus pending withdrawals (§15.7). Requesting a withdrawal drops availability
    /// immediately, so a `CreditAccount` carrying that drop can legitimately push the mirror
    /// below claims the engine already holds; §9.3's timelock is what makes that window safe
    /// rather than what prevents it. So this holds as a post-condition of *admission*, and
    /// not after a mirror update. The unconditional form spans both systems — balance plus
    /// locked escrow contributions — and lives in the harness, where §15's table puts it.
    ///
    /// # Errors
    ///
    /// [`InvariantViolation::ClaimCoverageBroken`].
    ///
    /// [`Engine::touched_accounts`]: crate::engine::Engine::touched_accounts
    pub fn check_claim_coverage_for(
        &self,
        account: AccountIdx,
    ) -> Result<(), InvariantViolation> {
        let Some(entry) = self.accounts.get(account.0 as usize) else { return Ok(()) };
        let claimed = entry
            .reserved()
            .checked_add(entry.committed())
            .ok_or(InvariantViolation::ClaimCoverageBroken(account))?;
        if claimed > entry.free() {
            return Err(InvariantViolation::ClaimCoverageBroken(account));
        }
        Ok(())
    }

    /// §15.6 — claim coverage: `∀ a: free(a) ≥ reserved(a) + committed(a)`.
    ///
    /// Against the **mirror**, because that is all the engine can see: the authoritative
    /// form compares against `custody.free(a)` and spans both systems, so it belongs to the
    /// harness once custody has balances (§2.2, §13.1). The mirror form is what the engine
    /// admits against, and `reserve` maintains it structurally — a violation here would mean
    /// a claim was created without passing `check_reservable`.
    ///
    /// # Errors
    ///
    /// [`InvariantViolation::ClaimCoverageBroken`] naming the account.
    ///
    /// Whole-table, so this is a **test and scenario** query — see
    /// [`check_claim_coverage_for`](Self::check_claim_coverage_for) for the per-command form.
    pub fn check_claim_coverage(&self) -> Result<(), InvariantViolation> {
        for (index, entry) in self.accounts.iter().enumerate() {
            let account = AccountIdx(u32::try_from(index).unwrap_or(u32::MAX));
            let claimed = entry
                .reserved()
                .checked_add(entry.committed())
                .ok_or(InvariantViolation::ClaimCoverageBroken(account))?;
            if claimed > entry.free() {
                return Err(InvariantViolation::ClaimCoverageBroken(account));
            }
        }
        Ok(())
    }

    /// §15.2 — immediately after `release_expired(account, now)`, no claim on that account's
    /// expiry chain has `expires_at <= now`.
    ///
    /// Scoped to the just-normalised account, deliberately. The global form is unsatisfiable:
    /// an untouched account holds its expired claims until something touches it, and that
    /// costs slab occupancy but never affects a decision, because no decision about A is
    /// taken without first normalising A (§4.3).
    ///
    /// # Errors
    ///
    /// [`InvariantViolation::ExpiredClaimAfterNormalisation`].
    pub fn check_normalised(
        &self,
        account: AccountIdx,
        now: Ts,
    ) -> Result<(), InvariantViolation> {
        let Some(entry) = self.accounts.get(account.0 as usize) else { return Ok(()) };
        let mut cursor = entry.expiry_head;
        let mut steps: u32 = 0;
        while let Some(at) = cursor.index() {
            let Some(reservation) = self.reservation_at(at) else { break };
            if reservation.is_expired_at(now) == Some(true) {
                return Err(InvariantViolation::ExpiredClaimAfterNormalisation(account));
            }
            cursor = reservation.neighbours().1;
            steps = steps.saturating_add(1);
            if steps > self.reservations.capacity() {
                break;
            }
        }
        Ok(())
    }

    /// Asserted after every ledger mutation, including inside a commit phase — so only the
    /// structural half, which is the half that is true at every step.
    fn assert_invariants(&self) {
        if cfg!(debug_assertions)
            && let Err(violation) = self.check_structural_invariants()
        {
            panic!("ledger invariant violated (SPEC §15): {violation:?}");
        }
    }

    fn assert_normalised(&self, account: AccountIdx, now: Ts) {
        if cfg!(debug_assertions)
            && let Err(violation) = self.check_normalised(account, now)
        {
            panic!("normalisation invariant violated (SPEC §15.2): {violation:?}");
        }
    }
}
