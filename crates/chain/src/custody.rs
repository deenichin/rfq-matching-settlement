//! The custody mock (SPEC §9).
//!
//! An in-process model of an escrow contract. It holds balances, escrows, nonces and the
//! withdrawal timelock. It holds no reference to the engine, knows nothing of requests,
//! quotes, legs, reservations or claims, and has never heard of a `committed` bucket
//! (§13.1). The only way in is a [`Bundle`] handed to it by the settlement adapter.
//!
//! **Custody validates against its own clock**, not the engine's (§9.1), sampled once per
//! settlement transaction (CLAUDE 2). In v1 the two are separate `Clock` instances that
//! happen to agree; the mock allows an offset so a test can demonstrate the divergence case.
//! Without a second clock that assumption is invisible in v1 and false in v2.
//!
//! # Balance and availability are different numbers
//!
//! | | Uses | Why |
//! |---|---|---|
//! | admission (§6) | **availability** = balance − pending withdrawals | forward-looking: never lend against money already on its way out |
//! | settlement (§9.1) | **balance** = what custody holds now | present-tense: is the money here for this transaction |
//!
//! If settlement validated availability, `RequestWithdrawal` would instantly kill every
//! basket already in flight — last look reintroduced through custody, which is exactly what
//! the §9.3 timelock exists to prevent. Worse, it would be invisible: the timelock would
//! still appear to function while every settlement quietly failed. The timelock's promise is
//! that the money remains *present* for as long as a quote can bind, so presence is what
//! settlement checks.

use std::collections::BTreeSet;

use rfq_core::account::AccountIdx;
use rfq_core::clock::Clock;
use rfq_core::escrow::EscrowId;
use rfq_core::request::Nonce;
use rfq_core::types::{Amount, Dur, Ts};

use crate::bundle::Bundle;
use crate::escrow::Escrow;

/// A hook fired on entry to a settlement transaction, before any validation.
///
/// Receives the ledger and the instant this transaction sampled, so everything it does is an
/// ordinary custody operation at an ordinary time — which is the point: it models another
/// transaction landing in the TOCTOU window, not a magic mutation.
pub type SettleEntryHook = Box<dyn FnMut(&mut CustodyLedger, Ts)>;

/// Why custody refused a balance operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CustodyError {
    /// The account index is outside the preallocated table.
    UnknownAccount,
    /// The withdrawal asks for more than is available — balance minus what is already on
    /// its way out.
    InsufficientAvailable,
    /// One pending withdrawal per account. A second is refused rather than merged, because
    /// merging would have to choose whose maturity date wins, and either choice is a policy
    /// nobody asked for.
    WithdrawalAlreadyPending,
    /// Nothing is pending for this account.
    NoWithdrawalPending,
    /// `now < requested_at + WITHDRAWAL_DELAY`. Execution is unconditional at maturity and
    /// impossible before it; **no quote or escrow is consulted either way** (§9.3).
    WithdrawalNotMatured,
    /// Checked arithmetic overflowed. A rejection, never a wrap (CLAUDE 14).
    AmountOverflow,
}

/// Why a settlement transaction reverted (SPEC §9.1).
///
/// A revert is wholesale: no leg settles, no balance moves, the nonce is not consumed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettleError {
    /// The nonce has already been consumed.
    ///
    /// **A retry bouncing off its own nonce is evidence the original succeeded** (§8.1).
    /// Reading this as "the settlement failed" is the duplication path.
    NonceReused,
    /// A leg's quote is expired **at custody's clock**. The engine may believe it live; the
    /// two clocks are separate and the chain's is the one that counts here (§9.1).
    QuoteExpired {
        /// Which leg.
        leg: u8,
    },
    /// An account's balance does not cover what this transaction would debit it.
    InsufficientFunds {
        /// Whose.
        account: AccountIdx,
    },
    /// No room left in the escrow table.
    EscrowCapacityExhausted,
    /// A leg names an account outside the preallocated table.
    UnknownAccount,
    /// A bundle with no legs.
    NoLegs,
    /// More legs than storage allows.
    TooManyLegs,
    /// Checked arithmetic overflowed.
    AmountOverflow,
}

/// What a settled transaction produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SettleReceipt {
    /// The escrows formed, one per leg, in leg order.
    pub escrows: [EscrowId; rfq_core::config::MAX_LEGS],
    /// How many are real.
    pub n_escrows: u8,
}

/// One account's custody balance.
///
/// Two numbers, not one, because §9.1's whole distinction is that admission and settlement
/// ask different questions of the same account.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(clippy::struct_field_names)] // `balance` is the field's name because it is the
// quantity's name; renaming it to satisfy the lint would obscure exactly what §9.1
// distinguishes it from.
pub struct Balance {
    /// What custody holds now. Reduced only by an executed withdrawal or a settlement
    /// debit — **not** by requesting a withdrawal.
    balance: Amount,
    /// Requested and not yet executed. Subtracted from availability immediately.
    pending: Amount,
    /// When the pending withdrawal matures. Meaningless when `pending` is zero.
    matures_at: Ts,
}

impl Balance {
    /// What custody holds now. This is what settlement validates (§9.1).
    #[must_use]
    pub const fn balance(&self) -> Amount {
        self.balance
    }

    /// `balance − pending`. This is what admission validates (§6).
    #[must_use]
    pub const fn available(&self) -> Amount {
        Amount(self.balance.0.saturating_sub(self.pending.0))
    }

    /// The amount on its way out, and when it lands.
    #[must_use]
    pub const fn pending_withdrawal(&self) -> Option<(Amount, Ts)> {
        if self.pending.0 == 0 { None } else { Some((self.pending, self.matures_at)) }
    }
}

/// The state a settlement transaction can touch.
///
/// Split from the clock and the test hook so that [`Custody::on_settle_entry`] can be handed
/// a mutable view of exactly this while custody itself is mid-transaction — modelling
/// another transaction landing between our pre-check and our inclusion.
#[derive(Debug)]
pub struct CustodyLedger {
    balances: Vec<Balance>,
    escrows: Vec<Escrow>,
    max_escrows: usize,
    nonces: BTreeSet<Nonce>,
    withdrawal_delay: Dur,
    deposited: Amount,
    withdrawn: Amount,
}

impl CustodyLedger {
    /// One account's balance, if the index resolves.
    #[must_use]
    pub fn account(&self, account: AccountIdx) -> Option<&Balance> {
        self.balances.get(account.0 as usize)
    }

    /// What custody holds for `account`.
    #[must_use]
    pub fn balance(&self, account: AccountIdx) -> Amount {
        self.account(account).map_or(Amount::ZERO, Balance::balance)
    }

    /// `balance − pending` for `account`.
    #[must_use]
    pub fn available(&self, account: AccountIdx) -> Amount {
        self.account(account).map_or(Amount::ZERO, Balance::available)
    }

    /// How many accounts the table holds.
    #[must_use]
    pub fn account_count(&self) -> u32 {
        u32::try_from(self.balances.len()).unwrap_or(u32::MAX)
    }

    /// Every deposit ever made.
    #[must_use]
    pub const fn deposited(&self) -> Amount {
        self.deposited
    }

    /// Every withdrawal ever executed.
    #[must_use]
    pub const fn withdrawn(&self) -> Amount {
        self.withdrawn
    }

    /// An escrow, if the id resolves.
    #[must_use]
    pub fn escrow(&self, id: EscrowId) -> Option<&Escrow> {
        self.escrows.get(id.0 as usize)
    }

    /// Every escrow, in the order they formed.
    pub fn escrows(&self) -> impl Iterator<Item = (EscrowId, &Escrow)> {
        self.escrows.iter().enumerate().filter_map(|(index, escrow)| {
            Some((EscrowId(u32::try_from(index).ok()?), escrow))
        })
    }

    /// Whether this nonce has been consumed.
    ///
    /// Monotonic and terminal: once consumed, always consumed. A nonce that has been used
    /// is a statement about the *nonce*, not about whichever submission most recently
    /// carried it (§8.1).
    #[must_use]
    pub fn nonce_used(&self, nonce: Nonce) -> bool {
        self.nonces.contains(&nonce)
    }

    /// Credit an account. The only way money enters custody.
    ///
    /// # Errors
    ///
    /// [`CustodyError::UnknownAccount`], [`CustodyError::AmountOverflow`].
    pub fn deposit(&mut self, account: AccountIdx, amount: Amount) -> Result<(), CustodyError> {
        let deposited =
            self.deposited.checked_add(amount).ok_or(CustodyError::AmountOverflow)?;
        let entry =
            self.balances.get_mut(account.0 as usize).ok_or(CustodyError::UnknownAccount)?;
        let balance = entry.balance.checked_add(amount).ok_or(CustodyError::AmountOverflow)?;
        entry.balance = balance;
        self.deposited = deposited;
        Ok(())
    }

    /// Mark funds as leaving. **Availability drops immediately; the balance does not**
    /// (§9.3).
    ///
    /// # Errors
    ///
    /// [`CustodyError::UnknownAccount`], [`CustodyError::WithdrawalAlreadyPending`],
    /// [`CustodyError::InsufficientAvailable`], [`CustodyError::AmountOverflow`].
    pub fn request_withdrawal(
        &mut self,
        account: AccountIdx,
        amount: Amount,
        now: Ts,
    ) -> Result<Ts, CustodyError> {
        let delay = self.withdrawal_delay;
        let entry =
            self.balances.get_mut(account.0 as usize).ok_or(CustodyError::UnknownAccount)?;
        if entry.pending.0 != 0 {
            return Err(CustodyError::WithdrawalAlreadyPending);
        }
        if amount > entry.available() {
            return Err(CustodyError::InsufficientAvailable);
        }
        let matures_at = now.checked_add(delay).ok_or(CustodyError::AmountOverflow)?;
        entry.pending = amount;
        entry.matures_at = matures_at;
        Ok(matures_at)
    }

    /// Execute a matured withdrawal: the balance finally moves.
    ///
    /// Execution is **unconditional at maturity**. No quote, escrow or basket is consulted:
    /// the timelock's whole promise is that money stays present for as long as a quote can
    /// bind, and a timelock that could be extended by outstanding obligations would be a
    /// lock nobody could reason about.
    ///
    /// # Errors
    ///
    /// [`CustodyError::UnknownAccount`], [`CustodyError::NoWithdrawalPending`],
    /// [`CustodyError::WithdrawalNotMatured`].
    pub fn execute_withdrawal(
        &mut self,
        account: AccountIdx,
        now: Ts,
    ) -> Result<Amount, CustodyError> {
        let entry =
            self.balances.get_mut(account.0 as usize).ok_or(CustodyError::UnknownAccount)?;
        if entry.pending.0 == 0 {
            return Err(CustodyError::NoWithdrawalPending);
        }
        // Half-open, like every other deadline in the design: matured at exactly the
        // maturity instant (§4.2).
        if now < entry.matures_at {
            return Err(CustodyError::WithdrawalNotMatured);
        }
        // Paid out of what is actually there. A settlement may have debited the account
        // since the request — custody has no concept of the engine's claims (§1), so a
        // maker may ask to withdraw capital that later goes into escrow. Paying out more
        // than the account holds would create money; `withdrawn` records what left.
        let amount = Amount(entry.pending.0.min(entry.balance.0));
        entry.balance = Amount(entry.balance.0.saturating_sub(amount.0));
        entry.pending = Amount::ZERO;
        self.withdrawn = Amount(self.withdrawn.0.saturating_add(amount.0));
        Ok(amount)
    }

    /// Execute every withdrawal that has matured. Returns how many.
    pub fn execute_matured_withdrawals(&mut self, now: Ts) -> u32 {
        let mut executed = 0_u32;
        for index in 0..self.balances.len() {
            let Ok(index) = u32::try_from(index) else { break };
            if self.execute_withdrawal(AccountIdx(index), now).is_ok() {
                executed = executed.saturating_add(1);
            }
        }
        executed
    }

    /// Pay an escrow out to `winner`, or refund both sides if `winner` is `None`.
    ///
    /// Re-settlement is a no-op, so replay is harmless (§9.2). The consumed flag is set in
    /// the same mutation as the credit.
    ///
    /// Present in S3 so conservation has a way back out of `Locked`; the outcome that
    /// decides `winner` is S5's.
    ///
    /// # Errors
    ///
    /// [`CustodyError::UnknownAccount`] if the escrow id does not resolve.
    pub fn settle_escrow(
        &mut self,
        id: EscrowId,
        winner: Option<AccountIdx>,
    ) -> Result<bool, CustodyError> {
        let escrow = *self.escrows.get(id.0 as usize).ok_or(CustodyError::UnknownAccount)?;
        if !escrow.is_locked() {
            return Ok(false);
        }
        let credits: [(AccountIdx, Amount); 2] = match winner {
            Some(winner) => [(winner, escrow.notional()), (winner, Amount::ZERO)],
            // Void returns each side its own contribution, restoring the exact pre-trade
            // allocation. Splitting the notional would move money between the parties
            // (§10.3).
            None => [
                (escrow.requester(), escrow.requester_contribution()),
                (escrow.maker(), escrow.maker_contribution()),
            ],
        };
        for (account, amount) in credits {
            if let Some(entry) = self.balances.get_mut(account.0 as usize) {
                entry.balance = Amount(entry.balance.0.saturating_add(amount.0));
            }
        }
        if let Some(entry) = self.escrows.get_mut(id.0 as usize) {
            *entry = Escrow::locked(
                escrow.contract(),
                escrow.side(),
                escrow.size(),
                escrow.requester(),
                escrow.maker(),
                escrow.requester_contribution(),
                escrow.maker_contribution(),
            );
            entry.mark_settled();
        }
        Ok(true)
    }
}

/// The escrow contract, as a local mock.
///
/// Generic over its clock so that production wiring takes a monotonic clock and tests take a
/// settable one. A custody built on a monotonic clock exposes no way to advance it, because
/// the type has no such method.
pub struct Custody<C: Clock> {
    clock: C,
    ledger: CustodyLedger,
    on_settle_entry: Option<SettleEntryHook>,
}

impl<C: Clock + core::fmt::Debug> core::fmt::Debug for Custody<C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Custody")
            .field("clock", &self.clock)
            .field("ledger", &self.ledger)
            .field("on_settle_entry", &self.on_settle_entry.is_some())
            .finish()
    }
}

impl<C: Clock> Custody<C> {
    /// Construct custody with its own clock, its timelock, and its preallocated tables.
    ///
    /// The delay is passed as a bare duration rather than as the venue's whole `Config`:
    /// custody enforces the timelock but has no business knowing how many legs a request may
    /// carry. The four-term inequality relating this delay to the engine's quote lifetime
    /// spans both systems and is asserted once, at startup, by whatever constructs them both
    /// (§9.3).
    #[must_use]
    pub fn new(clock: C, withdrawal_delay: Dur, max_accounts: u32, max_escrows: u32) -> Self {
        let mut balances = Vec::with_capacity(max_accounts as usize);
        balances.resize(max_accounts as usize, Balance::default());
        Self {
            clock,
            ledger: CustodyLedger {
                balances,
                escrows: Vec::with_capacity(max_escrows as usize),
                max_escrows: max_escrows as usize,
                nonces: BTreeSet::new(),
                withdrawal_delay,
                deposited: Amount::ZERO,
                withdrawn: Amount::ZERO,
            },
            on_settle_entry: None,
        }
    }

    /// Chain time. Sampled once per settlement transaction, never by the engine.
    pub fn now(&self) -> Ts {
        self.clock.now()
    }

    /// How long `RequestWithdrawal` waits before execution (§9.3).
    pub const fn withdrawal_delay(&self) -> Dur {
        self.ledger.withdrawal_delay
    }

    /// Custody's clock, for the harness to advance independently of the engine's.
    pub const fn clock_mut(&mut self) -> &mut C {
        &mut self.clock
    }

    /// Balances, escrows and nonces, read-only.
    pub const fn ledger(&self) -> &CustodyLedger {
        &self.ledger
    }

    /// Balances, escrows and nonces, for the operations that need no clock.
    pub const fn ledger_mut(&mut self) -> &mut CustodyLedger {
        &mut self.ledger
    }

    /// Credit an account.
    ///
    /// # Errors
    ///
    /// Any [`CustodyError`].
    pub fn deposit(&mut self, account: AccountIdx, amount: Amount) -> Result<(), CustodyError> {
        self.ledger.deposit(account, amount)
    }

    /// Request a withdrawal at custody's current time.
    ///
    /// # Errors
    ///
    /// Any [`CustodyError`].
    pub fn request_withdrawal(
        &mut self,
        account: AccountIdx,
        amount: Amount,
    ) -> Result<Ts, CustodyError> {
        let now = self.clock.now();
        self.ledger.request_withdrawal(account, amount, now)
    }

    /// Execute a matured withdrawal at custody's current time.
    ///
    /// # Errors
    ///
    /// Any [`CustodyError`].
    pub fn execute_withdrawal(&mut self, account: AccountIdx) -> Result<Amount, CustodyError> {
        let now = self.clock.now();
        self.ledger.execute_withdrawal(account, now)
    }

    /// Install a hook fired on entry to [`Custody::settle`], before any validation.
    ///
    /// Models another transaction landing between our pre-check and our inclusion — the
    /// window that makes checking-then-submitting TOCTOU (§8.2). It receives the ledger and
    /// the instant this transaction sampled, so everything it does is an ordinary custody
    /// operation at an ordinary time.
    pub fn on_settle_entry(&mut self, hook: SettleEntryHook) {
        self.on_settle_entry = Some(hook);
    }

    /// A read-only dry run of [`Custody::settle`].
    ///
    /// **An optimisation with no correctness role.** Checking then submitting is TOCTOU: the
    /// window between check and inclusion is exactly where a withdrawal lands. The
    /// authoritative validation is inside the transaction (§8.2), and the gate proves it by
    /// making a withdrawal land in that window.
    ///
    /// # Errors
    ///
    /// Whatever [`Custody::settle`] would have reverted with, as of now.
    pub fn precheck(&self, bundle: &Bundle) -> Result<(), SettleError> {
        let now = self.clock.now();
        Self::validate(&self.ledger, bundle, now).map(|_| ())
    }

    /// Include a bundle: validate **everything** against current chain state, then debit all
    /// and form escrows, or revert entirely (§9.1).
    ///
    /// Same plan-check-commit discipline one layer down. This mirrors EVM semantics — a
    /// transaction is atomic and reverts wholesale — which is why multi-leg atomicity at the
    /// custody layer is **inherited, not built**, and why the interface is a single
    /// `submit(bundle)` rather than per-leg calls.
    ///
    /// # Errors
    ///
    /// Any [`SettleError`]. On a revert nothing moved and the nonce was not consumed.
    pub fn settle(&mut self, bundle: &Bundle) -> Result<SettleReceipt, SettleError> {
        // Sampled once per settlement transaction, from custody's own clock (§9.1).
        let now = self.clock.now();

        // Anything landing here is landing between somebody's pre-check and this inclusion.
        if let Some(hook) = self.on_settle_entry.as_mut() {
            hook(&mut self.ledger, now);
        }

        // ── PLAN / CHECK ── everything, against state as it is *now*, not as it was.
        let debits = Self::validate(&self.ledger, bundle, now)?;
        if self
            .ledger
            .escrows
            .len()
            .checked_add(usize::from(bundle.n_legs))
            .is_none_or(|total| total > self.ledger.max_escrows)
        {
            return Err(SettleError::EscrowCapacityExhausted);
        }

        // ── COMMIT ── no fallible operation past this line.
        for (account, amount) in debits.entries() {
            if let Some(entry) = self.ledger.balances.get_mut(account.0 as usize) {
                entry.balance = Amount(entry.balance.0.saturating_sub(amount.0));
            }
        }
        let mut escrows = [EscrowId(0); rfq_core::config::MAX_LEGS];
        for (index, leg) in bundle.legs().iter().enumerate() {
            let id = EscrowId(u32::try_from(self.ledger.escrows.len()).unwrap_or(u32::MAX));
            self.ledger.escrows.push(Escrow::locked(
                leg.contract,
                leg.side,
                leg.size,
                bundle.requester,
                leg.maker,
                leg.requester_contribution().unwrap_or(Amount::ZERO),
                leg.maker_contribution().unwrap_or(Amount::ZERO),
            ));
            if let Some(slot) = escrows.get_mut(index) {
                *slot = id;
            }
        }
        self.ledger.nonces.insert(bundle.nonce);

        Ok(SettleReceipt { escrows, n_escrows: bundle.n_legs })
    }

    /// Every check of §9.1, in order, returning what each account would be debited.
    ///
    /// Shared by `precheck` and `settle` so the two cannot drift — but running it twice is
    /// not what makes settlement safe. Only the second run counts.
    fn validate(
        ledger: &CustodyLedger,
        bundle: &Bundle,
        now: Ts,
    ) -> Result<Debits, SettleError> {
        if bundle.n_legs == 0 {
            return Err(SettleError::NoLegs);
        }
        if usize::from(bundle.n_legs) > rfq_core::config::MAX_LEGS {
            return Err(SettleError::TooManyLegs);
        }
        if ledger.nonce_used(bundle.nonce) {
            return Err(SettleError::NonceReused);
        }

        // Debits are accumulated per account before any is checked. A maker winning two legs
        // must cover the sum, not each leg independently — checking them separately would
        // pass on a balance that covers either one alone.
        let mut debits = Debits::default();
        for (index, leg) in bundle.legs().iter().enumerate() {
            let leg_id = u8::try_from(index).unwrap_or(u8::MAX);
            // Custody's clock, not the engine's. A quote the engine believes live may be
            // expired here, and here is where it counts.
            if now >= leg.quote_expiry {
                return Err(SettleError::QuoteExpired { leg: leg_id });
            }
            let maker = leg.maker_contribution().ok_or(SettleError::AmountOverflow)?;
            let requester = leg.requester_contribution().ok_or(SettleError::AmountOverflow)?;
            debits.add(leg.maker, maker).ok_or(SettleError::AmountOverflow)?;
            debits.add(bundle.requester, requester).ok_or(SettleError::AmountOverflow)?;
        }

        for (account, amount) in debits.entries() {
            if ledger.account(account).is_none() {
                return Err(SettleError::UnknownAccount);
            }
            // **Balance, not availability.** A pending withdrawal does not kill a basket
            // already in flight; the timelock is what keeps the money present until it can
            // no longer be needed.
            if ledger.balance(account) < amount {
                return Err(SettleError::InsufficientFunds { account });
            }
        }

        Ok(debits)
    }
}

/// Per-account debits, accumulated on the stack.
///
/// `MAX_LEGS` makers plus one requester is the most a bundle can name, so the table is fixed
/// and nothing allocates.
#[derive(Debug, Default)]
struct Debits {
    accounts: [Option<(AccountIdx, Amount)>; rfq_core::config::MAX_LEGS + 1],
}

impl Debits {
    fn add(&mut self, account: AccountIdx, amount: Amount) -> Option<()> {
        for slot in &mut self.accounts {
            match slot {
                Some((existing, total)) if *existing == account => {
                    *total = total.checked_add(amount)?;
                    return Some(());
                }
                Some(_) => {}
                None => {
                    *slot = Some((account, amount));
                    return Some(());
                }
            }
        }
        None
    }

    fn entries(&self) -> impl Iterator<Item = (AccountIdx, Amount)> + '_ {
        self.accounts.iter().flatten().copied()
    }
}
