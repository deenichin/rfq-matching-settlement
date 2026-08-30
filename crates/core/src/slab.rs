//! The generation-counted slab (SPEC §3).
//!
//! Every slab in the engine — requests, quotes, reservations, contracts — is an instance
//! of this one type, so **every slab carries a generation counter** by construction rather
//! than by four separate decisions that could each be forgotten. Two things depend on that:
//!
//! - a stale handle naming a slot that has been freed and reissued must be *rejected*, not
//!   silently resolved to its successor (SPEC §15.3: a failed dereference is not a passing
//!   check, and index reuse alone proves nothing);
//! - the settlement nonce is `(ReqIdx, req_generation)` taken from the **request** slab's
//!   generation counter (SPEC §8.1). A request slot reused after its predecessor was freed
//!   yields a different generation, so nonces are never reused even though indices are.
//!   That is the whole reason the nonce needs no hashing, which §3 forbids inside `apply`.
//!
//! Slabs are preallocated to configured capacity and **never grow** (CLAUDE 9, 10).
//! Exhaustion is [`SlabFull`], a rejection with its own error variant, never a
//! reallocation. `capacity()` is fixed at construction and is the observable proxy for
//! allocation discipline that CLAUDE 25 asks for in place of an allocator hook.

use core::marker::PhantomData;

/// The nil link. Reserved, so capacity is bounded by `u32::MAX - 1` slots.
const NIL: u32 = u32::MAX;

/// A handle into a [`Slab`], carrying the generation of the slot it was issued against.
///
/// Typed by the slot contents, so a request handle cannot be passed to a quote slab.
/// `Copy` regardless of `T`; the `PhantomData<fn() -> T>` marker owns nothing.
pub struct Handle<T> {
    index: u32,
    generation: u32,
    slot_type: PhantomData<fn() -> T>,
}

impl<T> Handle<T> {
    /// The dense index this handle names. Stable for the life of the entry.
    #[must_use]
    pub const fn index(&self) -> u32 {
        self.index
    }

    /// The generation this handle was issued against.
    ///
    /// For a request handle this is the second half of the settlement nonce (SPEC §8.1);
    /// it is what makes a resubmission of an already-included bundle bounce off its own
    /// nonce instead of forming a second escrow.
    #[must_use]
    pub const fn generation(&self) -> u32 {
        self.generation
    }
}

// Derived impls would demand `T: Clone`/`T: Debug`; a handle is a pair of integers and
// needs nothing of its referent.
impl<T> Clone for Handle<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Handle<T> {}
impl<T> PartialEq for Handle<T> {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index && self.generation == other.generation
    }
}
impl<T> Eq for Handle<T> {}
impl<T> core::fmt::Debug for Handle<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Handle({}#{})", self.index, self.generation)
    }
}

/// The slab is at its configured capacity. A rejection, never a reallocation (CLAUDE 10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlabFull;

/// What a slot holds. A vacant slot carries the free-list link in place of its value, so
/// the free list costs no storage of its own.
#[derive(Debug)]
enum SlotState<T> {
    Vacant { next: u32 },
    Occupied(T),
}

#[derive(Debug)]
struct Slot<T> {
    generation: u32,
    state: SlotState<T>,
}

/// A preallocated, generation-counted slab with an intrusive free list.
#[derive(Debug)]
pub struct Slab<T> {
    slots: Vec<Slot<T>>,
    free_head: u32,
    live: u32,
}

impl<T> Slab<T> {
    /// Preallocate `capacity` slots. This is the only allocation the slab ever performs.
    ///
    /// # Panics
    ///
    /// If `capacity` is `u32::MAX`, which is reserved as the nil free-list link. Slab
    /// capacities come from [`Config`](crate::config::Config) and are validated at
    /// startup, so this is a startup failure and never reachable from `apply`.
    #[must_use]
    pub fn with_capacity(capacity: u32) -> Self {
        assert!(capacity < NIL, "slab capacity must be < u32::MAX (u32::MAX is the nil link)");
        let mut slots = Vec::with_capacity(capacity as usize);
        for index in 0..capacity {
            // Link each slot to its successor so allocation walks the slab in order and
            // the free list needs no separate storage.
            let next = index.checked_add(1).filter(|next| *next < capacity).unwrap_or(NIL);
            slots.push(Slot { generation: 0, state: SlotState::Vacant { next } });
        }
        let free_head = if capacity == 0 { NIL } else { 0 };
        Self { slots, free_head, live: 0 }
    }

    /// Slots allocated at construction. Never changes — this is the allocation proxy of
    /// CLAUDE 25, and a test asserts it is unchanged after N randomised operations.
    #[must_use]
    pub fn capacity(&self) -> u32 {
        // `slots` is filled once in `with_capacity` and neither pushed to nor truncated.
        u32::try_from(self.slots.len()).unwrap_or(NIL)
    }

    /// Occupied slots.
    #[must_use]
    pub const fn len(&self) -> u32 {
        self.live
    }

    /// Whether any slot is occupied.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// Take a free slot, or reject.
    ///
    /// # Errors
    ///
    /// [`SlabFull`] when no slot is free. The slab does not grow (CLAUDE 10).
    pub fn insert(&mut self, value: T) -> Result<Handle<T>, SlabFull> {
        let index = self.free_head;
        let Some(slot) = self.slots.get_mut(index as usize) else {
            // `free_head == NIL`, or a link that cannot be dereferenced. Both mean the
            // same thing to the caller and neither may allocate.
            return Err(SlabFull);
        };
        let SlotState::Vacant { next } = slot.state else {
            debug_assert!(false, "free list points at an occupied slot");
            return Err(SlabFull);
        };
        slot.state = SlotState::Occupied(value);
        let generation = slot.generation;
        self.free_head = next;
        self.live = self.live.saturating_add(1);
        Ok(Handle { index, generation, slot_type: PhantomData })
    }

    /// The entry `handle` names, or `None` if the handle is stale.
    ///
    /// Stale means either generation mismatch — the slot was freed, so this handle refers
    /// to an entry that no longer exists even if the index now holds something else — or
    /// a vacant slot, which a retired slot can still match on generation.
    #[must_use]
    pub fn get(&self, handle: Handle<T>) -> Option<&T> {
        match self.resolve(handle)? {
            SlotState::Occupied(value) => Some(value),
            SlotState::Vacant { .. } => None,
        }
    }

    /// The entry `handle` names, mutably, or `None` if the handle is stale.
    pub fn get_mut(&mut self, handle: Handle<T>) -> Option<&mut T> {
        let slot = self.slots.get_mut(handle.index as usize)?;
        if slot.generation != handle.generation {
            return None;
        }
        match &mut slot.state {
            SlotState::Occupied(value) => Some(value),
            SlotState::Vacant { .. } => None,
        }
    }

    /// Whether `handle` still names a live entry.
    #[must_use]
    pub fn contains(&self, handle: Handle<T>) -> bool {
        self.get(handle).is_some()
    }

    /// Free the slot `handle` names and return its contents, or `None` if the handle is
    /// stale — in which case **nothing is mutated**. A stale free must not evict whatever
    /// occupies the slot now; that is the same aliasing bug the generation exists to catch,
    /// and it would be silent.
    pub fn remove(&mut self, handle: Handle<T>) -> Option<T> {
        let free_head = self.free_head;
        let slot = self.slots.get_mut(handle.index as usize)?;
        if slot.generation != handle.generation {
            return None;
        }
        if matches!(slot.state, SlotState::Vacant { .. }) {
            return None;
        }

        // Bump first: every handle outstanding against this slot must stop resolving.
        // On overflow the slot is *retired* rather than wrapped — a wrapped generation
        // would let a handle from 2^32 reuses ago alias a live entry, and the whole point
        // of the counter is that it cannot. Retirement costs one slot of capacity in a
        // scenario that will not occur, and is the only failure mode worth having here.
        let reusable = match slot.generation.checked_add(1) {
            Some(next_generation) => {
                slot.generation = next_generation;
                true
            }
            None => false,
        };

        let next = if reusable { free_head } else { NIL };
        let previous = core::mem::replace(&mut slot.state, SlotState::Vacant { next });
        if reusable {
            self.free_head = handle.index;
        }
        self.live = self.live.saturating_sub(1);

        match previous {
            SlotState::Occupied(value) => Some(value),
            SlotState::Vacant { .. } => None,
        }
    }

    /// The handle naming whatever occupies `index`, or `None` if the slot is vacant.
    ///
    /// Intrusive chains are threaded with bare `u32` links (§4.3), because a link the
    /// structure maintains itself cannot go stale — if it could, the structure is already
    /// corrupt and a generation would not save it. Walking such a chain and then checking a
    /// *cross-structure* reference against it needs the full handle, and this recovers it.
    #[must_use]
    pub fn handle_at(&self, index: u32) -> Option<Handle<T>> {
        let slot = self.slots.get(index as usize)?;
        match slot.state {
            SlotState::Occupied(_) => {
                Some(Handle { index, generation: slot.generation, slot_type: PhantomData })
            }
            SlotState::Vacant { .. } => None,
        }
    }

    /// Every occupied slot, in ascending index order.
    ///
    /// Dense index order, so iteration is deterministic and may influence state or emitted
    /// events — unlike a hash container, which rule 3 bans for exactly that reason.
    pub fn iter(&self) -> impl Iterator<Item = (Handle<T>, &T)> {
        self.slots.iter().enumerate().filter_map(|(index, slot)| {
            let index = u32::try_from(index).ok()?;
            match &slot.state {
                SlotState::Occupied(value) => Some((
                    Handle { index, generation: slot.generation, slot_type: PhantomData },
                    value,
                )),
                SlotState::Vacant { .. } => None,
            }
        })
    }

    fn resolve(&self, handle: Handle<T>) -> Option<&SlotState<T>> {
        let slot = self.slots.get(handle.index as usize)?;
        (slot.generation == handle.generation).then_some(&slot.state)
    }
}
