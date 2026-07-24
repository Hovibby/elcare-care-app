// storage.rs
use crate::types::{Auction, BidRecord, Listing, Offer};
use soroban_sdk::{contracttype, Address, Env, Vec};

/// Identifies one of the growing id-collections kept by the marketplace.
///
/// Every index is stored as a sequence of fixed-capacity pages
/// (`DataKey::IndexPage(id, page_no)`) plus a single length key
/// (`DataKey::IndexLen(id)`), so no individual storage entry grows unboundedly
/// with protocol usage.  Page count is derived from the length
/// (`ceil(len / INDEX_PAGE_SIZE)`), so no separate page-count key is needed.
#[contracttype]
#[derive(Clone)]
pub enum IndexId {
    /// Global set of currently-active listing ids (supports swap-removal).
    ActiveListings,
    /// All listing ids ever created by an artist (append-only).
    ArtistListings(Address),
    /// All auction ids ever created by an artist (append-only).
    ArtistAuctions(Address),
    /// All offer ids ever made by an offerer (append-only).
    OffererOffers(Address),
    /// All offer ids ever made on a listing (append-only).
    ListingOffers(u64),
}

/// Resumable progress marker for a versioned storage migration.
#[contracttype]
#[derive(Clone)]
pub struct MigrationProgress {
    /// Which migration phase is in progress (see `contract::migrate_step`).
    pub phase: u32,
    /// Position within the phase (last fully-processed item id/index).
    pub cursor: u64,
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    ListingCount,
    Listing(u64),
    /// LEGACY (pre-1.1.0): monolithic `Vec<u64>` per-artist listing index.
    /// Superseded by `IndexPage(IndexId::ArtistListings(..), _)`; only read by
    /// `migrate` and removed once migrated.
    ArtistListings(Address),
    Admin,
    TokenWhitelist,
    Treasury,
    ProtocolFeeBps,
    AuctionCount,
    Auction(u64),
    /// LEGACY (pre-1.1.0): monolithic per-artist auction index (see above).
    ArtistAuctions(Address),
    RevokedArtist(Address),
    OfferCount,
    Offer(u64),
    /// LEGACY (pre-1.1.0): monolithic per-listing offer index (see above).
    ListingOffers(u64),
    /// LEGACY (pre-1.1.0): monolithic per-offerer offer index (see above).
    OffererOffers(Address),
    ListingLock(u64),
    AuctionLock(u64),
    IsPaused,
    PendingAdmin,
    /// LEGACY (pre-1.1.0): monolithic active-listings index (see above).
    ActiveListings,
    MinBidIncrement,
    AuctionExtensionWindow,
    AuctionExtensionTrigger,
    AuctionBids(u64),
    MinPrice,
    MaxPrice,
    MigrationDone(soroban_sdk::String),
    /// One fixed-capacity page (`Vec<u64>`, at most `INDEX_PAGE_SIZE` entries)
    /// of the identified index.
    IndexPage(IndexId, u32),
    /// Total number of elements stored across all pages of the index.
    IndexLen(IndexId),
    /// Current position of an active listing inside the ActiveListings index,
    /// enabling O(1) swap-removal.  Exists iff the listing is in the index.
    ActiveListingPos(u64),
    /// Bounded (≤ MAX_OFFERS_PER_LISTING) list of the listing's *Pending* offer
    /// ids.  Its length is the pending-offer counter used by `make_offer` for
    /// O(1) cap enforcement; entries are removed on every terminal transition.
    ListingPendingOffers(u64),
    /// Resume position for the batched `cancel_artist_listings` operation:
    /// number of entries of the artist-listings index already processed.
    ArtistCancelCursor(Address),
    /// Resumable progress of the versioned `migrate`/`migrate_step` operation.
    MigrationCursor(soroban_sdk::String),
    /// Admin-configurable global default for new-auction bid history cap.
    /// Stored as u32; defaults to `DEFAULT_BID_HISTORY_CAP` when absent.
    BidHistoryCap,
}

pub const LEDGER_TTL_BUMP: u32 = 432_000;
pub const LEDGER_TTL_THRESHOLD: u32 = 144_000;
pub const REENTRANCY_LOCK_TTL: u32 = 100;

pub fn bump_entry_ttl(env: &Env, key: &DataKey) {
    env.storage()
        .persistent()
        .extend_ttl(key, LEDGER_TTL_THRESHOLD, LEDGER_TTL_BUMP);
}

// ── Paged index engine ───────────────────────────────────────
//
// Each `IndexId` collection is a sequence of fixed-capacity pages.  Element
// `i` lives in page `i / INDEX_PAGE_SIZE` at offset `i % INDEX_PAGE_SIZE`.
// Appending touches only the last page; swap-removal touches at most the
// page holding the removed slot plus the last page.  Emptied pages are
// deleted so dead keys do not accumulate.

/// Maximum number of ids held by one index page.
pub const INDEX_PAGE_SIZE: u32 = 100;

fn index_page_key(id: &IndexId, page: u32) -> DataKey {
    DataKey::IndexPage(id.clone(), page)
}

fn index_len_key(id: &IndexId) -> DataKey {
    DataKey::IndexLen(id.clone())
}

/// Total number of elements in the index.
pub fn index_len(env: &Env, id: &IndexId) -> u32 {
    let key = index_len_key(id);
    let len = env
        .storage()
        .persistent()
        .get::<DataKey, u32>(&key)
        .unwrap_or(0);
    // Keep the length entry alive alongside its pages — it is read on every
    // index access, making it the hottest entry of the index.
    if len > 0 {
        bump_entry_ttl(env, &key);
    }
    len
}

fn set_index_len(env: &Env, id: &IndexId, len: u32) {
    let key = index_len_key(id);
    if len == 0 {
        env.storage().persistent().remove(&key);
    } else {
        env.storage().persistent().set(&key, &len);
        bump_entry_ttl(env, &key);
    }
}

/// Load one page of the index (empty vec if the page does not exist).
pub fn index_load_page(env: &Env, id: &IndexId, page: u32) -> Vec<u64> {
    let key = index_page_key(id, page);
    let value = env
        .storage()
        .persistent()
        .get::<DataKey, Vec<u64>>(&key)
        .unwrap_or_else(|| Vec::new(env));
    if !value.is_empty() {
        bump_entry_ttl(env, &key);
    }
    value
}

fn index_store_page(env: &Env, id: &IndexId, page: u32, entries: &Vec<u64>) {
    let key = index_page_key(id, page);
    if entries.is_empty() {
        // Dead pages are removed as soon as they empty out.
        env.storage().persistent().remove(&key);
    } else {
        env.storage().persistent().set(&key, entries);
        bump_entry_ttl(env, &key);
    }
}

/// Append `value` to the end of the index. O(1): reads/writes only the last
/// page and the length key.
pub fn index_append(env: &Env, id: &IndexId, value: u64) {
    let len = index_len(env, id);
    let page = len / INDEX_PAGE_SIZE;
    let mut entries = index_load_page(env, id, page);
    entries.push_back(value);
    index_store_page(env, id, page, &entries);
    set_index_len(env, id, len + 1);
}

/// Read the element at logical position `pos`, or `None` when out of range.
pub fn index_get(env: &Env, id: &IndexId, pos: u32) -> Option<u64> {
    if pos >= index_len(env, id) {
        return None;
    }
    index_load_page(env, id, pos / INDEX_PAGE_SIZE).get(pos % INDEX_PAGE_SIZE)
}

/// Remove the element at logical position `pos` by moving the last element of
/// the index into its slot (swap-remove).  Returns `Some(moved_value)` when an
/// element was relocated into `pos`, `None` when `pos` was the last element.
///
/// NOTE: this deliberately does not preserve insertion order — the caller must
/// treat the index as an unordered set once removals occur.
pub fn index_swap_remove(env: &Env, id: &IndexId, pos: u32) -> Option<u64> {
    let len = index_len(env, id);
    if pos >= len {
        return None;
    }
    let last = len - 1;
    let last_page_no = last / INDEX_PAGE_SIZE;
    let last_off = last % INDEX_PAGE_SIZE;
    let mut last_page = index_load_page(env, id, last_page_no);
    let last_val = last_page.get(last_off).unwrap();

    let moved = if pos == last {
        last_page.remove(last_off);
        index_store_page(env, id, last_page_no, &last_page);
        None
    } else {
        let pos_page_no = pos / INDEX_PAGE_SIZE;
        let pos_off = pos % INDEX_PAGE_SIZE;
        if pos_page_no == last_page_no {
            last_page.set(pos_off, last_val);
            last_page.remove(last_off);
            index_store_page(env, id, last_page_no, &last_page);
        } else {
            last_page.remove(last_off);
            index_store_page(env, id, last_page_no, &last_page);
            let mut pos_page = index_load_page(env, id, pos_page_no);
            pos_page.set(pos_off, last_val);
            index_store_page(env, id, pos_page_no, &pos_page);
        }
        Some(last_val)
    };
    set_index_len(env, id, last);
    moved
}

/// Read up to `limit` elements starting at logical position `start`.
/// Positions past the end yield an empty vector.
pub fn index_range(env: &Env, id: &IndexId, start: u32, limit: u32) -> Vec<u64> {
    let mut out = Vec::new(env);
    let len = index_len(env, id);
    if start >= len || limit == 0 {
        return out;
    }
    let end = start.saturating_add(limit).min(len);
    let mut page_no = start / INDEX_PAGE_SIZE;
    let mut entries = index_load_page(env, id, page_no);
    for pos in start..end {
        let p = pos / INDEX_PAGE_SIZE;
        if p != page_no {
            page_no = p;
            entries = index_load_page(env, id, page_no);
        }
        out.push_back(entries.get(pos % INDEX_PAGE_SIZE).unwrap());
    }
    out
}

/// Read the whole index.  Unbounded in the number of pages — reserved for
/// view functions and tests; transaction paths must use `index_range`.
pub fn index_all(env: &Env, id: &IndexId) -> Vec<u64> {
    index_range(env, id, 0, index_len(env, id))
}

// ── Counters ─────────────────────────────────────────────────

pub fn get_listing_count(env: &Env) -> u64 {
    env.storage()
        .persistent()
        .get::<DataKey, u64>(&DataKey::ListingCount)
        .unwrap_or(0)
}

pub fn increment_listing_count(env: &Env) -> u64 {
    let count = get_listing_count(env) + 1;
    env.storage()
        .persistent()
        .set(&DataKey::ListingCount, &count);
    bump_entry_ttl(env, &DataKey::ListingCount);
    count
}

pub fn get_auction_count(env: &Env) -> u64 {
    env.storage()
        .persistent()
        .get::<DataKey, u64>(&DataKey::AuctionCount)
        .unwrap_or(0)
}

pub fn increment_auction_count(env: &Env) -> u64 {
    let count = get_auction_count(env) + 1;
    env.storage()
        .persistent()
        .set(&DataKey::AuctionCount, &count);
    bump_entry_ttl(env, &DataKey::AuctionCount);
    count
}

pub fn get_offer_count(env: &Env) -> u64 {
    env.storage()
        .persistent()
        .get::<DataKey, u64>(&DataKey::OfferCount)
        .unwrap_or(0)
}

pub fn increment_offer_count(env: &Env) -> u64 {
    let count = get_offer_count(env) + 1;
    env.storage().persistent().set(&DataKey::OfferCount, &count);
    bump_entry_ttl(env, &DataKey::OfferCount);
    count
}

// ── CRUD ─────────────────────────────────────────────────────

pub fn save_listing(env: &Env, listing: &Listing) {
    let key = DataKey::Listing(listing.listing_id);
    env.storage().persistent().set(&key, listing);
    bump_entry_ttl(env, &key);
}

pub fn load_listing(env: &Env, listing_id: u64) -> Option<Listing> {
    let key = DataKey::Listing(listing_id);
    let res = env.storage().persistent().get::<DataKey, Listing>(&key);
    if res.is_some() {
        bump_entry_ttl(env, &key);
    }
    res
}

pub fn save_auction(env: &Env, auction: &Auction) {
    let key = DataKey::Auction(auction.auction_id);
    env.storage().persistent().set(&key, auction);
    bump_entry_ttl(env, &key);
}

pub fn load_auction(env: &Env, auction_id: u64) -> Option<Auction> {
    let key = DataKey::Auction(auction_id);
    let res = env.storage().persistent().get::<DataKey, Auction>(&key);
    if res.is_some() {
        bump_entry_ttl(env, &key);
    }
    res
}

pub fn save_offer(env: &Env, offer: &Offer) {
    let key = DataKey::Offer(offer.offer_id);
    env.storage().persistent().set(&key, offer);
    bump_entry_ttl(env, &key);
}

pub fn load_offer(env: &Env, offer_id: u64) -> Option<Offer> {
    let key = DataKey::Offer(offer_id);
    let res = env.storage().persistent().get::<DataKey, Offer>(&key);
    if res.is_some() {
        bump_entry_ttl(env, &key);
    }
    res
}

// ── Indices (paged) ──────────────────────────────────────────

pub fn add_artist_listing_id(env: &Env, artist: &Address, listing_id: u64) {
    index_append(env, &IndexId::ArtistListings(artist.clone()), listing_id);
}

pub fn get_artist_listing_ids(env: &Env, artist: &Address) -> Vec<u64> {
    index_all(env, &IndexId::ArtistListings(artist.clone()))
}

// ── Active listings index ────────────────────────────────────
//
// The only index that shrinks.  A per-listing position key
// (`ActiveListingPos`) makes removal O(1): read the position, swap the last
// element into the vacated slot, fix up the moved element's position key.
// Consequence (deliberate, documented): once removals occur, the index is an
// unordered set — pagination order is stable between removals but is no
// longer strict insertion order.

pub fn add_to_active_listings(env: &Env, listing_id: u64) {
    let idx = IndexId::ActiveListings;
    let pos_key = DataKey::ActiveListingPos(listing_id);
    // Idempotency guard: never double-insert an id already in the index.
    if env.storage().persistent().has(&pos_key) {
        return;
    }
    let pos = index_len(env, &idx);
    index_append(env, &idx, listing_id);
    env.storage().persistent().set(&pos_key, &pos);
    bump_entry_ttl(env, &pos_key);
}

pub fn remove_from_active_listings(env: &Env, listing_id: u64) {
    let idx = IndexId::ActiveListings;
    let pos_key = DataKey::ActiveListingPos(listing_id);
    let pos = match env.storage().persistent().get::<DataKey, u32>(&pos_key) {
        Some(p) => p,
        None => return, // not in the index — nothing to do
    };
    // Defensive consistency check: the slot must actually hold this id.
    if index_get(env, &idx, pos) != Some(listing_id) {
        return;
    }
    if let Some(moved) = index_swap_remove(env, &idx, pos) {
        let moved_key = DataKey::ActiveListingPos(moved);
        env.storage().persistent().set(&moved_key, &pos);
        bump_entry_ttl(env, &moved_key);
    }
    env.storage().persistent().remove(&pos_key);
}

pub fn active_listings_len(env: &Env) -> u32 {
    index_len(env, &IndexId::ActiveListings)
}

pub fn get_active_listing_ids_range(env: &Env, start: u32, limit: u32) -> Vec<u64> {
    index_range(env, &IndexId::ActiveListings, start, limit)
}

/// Whole active index — used by tests and migration assertions only; the
/// contract's read surface pages through `get_active_listing_ids_range`.
#[allow(dead_code)]
pub fn get_active_listing_ids(env: &Env) -> Vec<u64> {
    index_all(env, &IndexId::ActiveListings)
}

pub fn add_artist_auction_id(env: &Env, artist: &Address, auction_id: u64) {
    index_append(env, &IndexId::ArtistAuctions(artist.clone()), auction_id);
}

pub fn get_artist_auction_ids(env: &Env, artist: &Address) -> Vec<u64> {
    index_all(env, &IndexId::ArtistAuctions(artist.clone()))
}

pub fn add_listing_offer_id(env: &Env, listing_id: u64, offer_id: u64) {
    index_append(env, &IndexId::ListingOffers(listing_id), offer_id);
}

pub fn load_listing_offers(env: &Env, listing_id: u64) -> Vec<u64> {
    index_all(env, &IndexId::ListingOffers(listing_id))
}

pub fn add_offerer_offer_id(env: &Env, offerer: &Address, offer_id: u64) {
    index_append(env, &IndexId::OffererOffers(offerer.clone()), offer_id);
}

pub fn load_offerer_offers(env: &Env, offerer: &Address) -> Vec<u64> {
    index_all(env, &IndexId::OffererOffers(offerer.clone()))
}

// ── Pending-offer tracking ───────────────────────────────────
//
// A single bounded entry per listing (≤ MAX_OFFERS_PER_LISTING ids).  Its
// length is the pending-offer counter: `make_offer` enforces the cap with one
// storage read instead of loading every historical offer.  Every terminal
// transition (accept / reject / withdraw / reclaim / auto-reject during
// buy_artwork or cancellation) removes the offer id here, and the refund
// sweeps iterate this bounded list instead of the full per-listing history.

pub fn load_pending_offer_ids(env: &Env, listing_id: u64) -> Vec<u64> {
    let key = DataKey::ListingPendingOffers(listing_id);
    let value = env
        .storage()
        .persistent()
        .get::<DataKey, Vec<u64>>(&key)
        .unwrap_or_else(|| Vec::new(env));
    if !value.is_empty() {
        bump_entry_ttl(env, &key);
    }
    value
}

/// Number of currently-Pending offers on the listing (O(1) storage reads).
pub fn pending_offer_count(env: &Env, listing_id: u64) -> u32 {
    load_pending_offer_ids(env, listing_id).len()
}

pub fn add_pending_offer(env: &Env, listing_id: u64, offer_id: u64) {
    let key = DataKey::ListingPendingOffers(listing_id);
    let mut ids = load_pending_offer_ids(env, listing_id);
    ids.push_back(offer_id);
    env.storage().persistent().set(&key, &ids);
    bump_entry_ttl(env, &key);
}

/// Remove `offer_id` from the listing's pending set.  No-op when absent (e.g.
/// offers created before the 1.1.0 migration ran).  The entry is deleted when
/// the last pending offer leaves.
pub fn remove_pending_offer(env: &Env, listing_id: u64, offer_id: u64) {
    let key = DataKey::ListingPendingOffers(listing_id);
    let ids = load_pending_offer_ids(env, listing_id);
    if let Some(i) = ids.first_index_of(offer_id) {
        let mut updated = ids;
        updated.remove(i);
        if updated.is_empty() {
            env.storage().persistent().remove(&key);
        } else {
            env.storage().persistent().set(&key, &updated);
            bump_entry_ttl(env, &key);
        }
    }
}

/// Drop the whole pending set (used when a listing reaches a terminal state
/// and all its pending offers were swept in the same invocation).
pub fn clear_pending_offers(env: &Env, listing_id: u64) {
    env.storage()
        .persistent()
        .remove(&DataKey::ListingPendingOffers(listing_id));
}

// ── Batched cancel_artist_listings cursor ────────────────────

pub fn get_artist_cancel_cursor(env: &Env, artist: &Address) -> u32 {
    env.storage()
        .persistent()
        .get::<DataKey, u32>(&DataKey::ArtistCancelCursor(artist.clone()))
        .unwrap_or(0)
}

pub fn set_artist_cancel_cursor(env: &Env, artist: &Address, cursor: u32) {
    let key = DataKey::ArtistCancelCursor(artist.clone());
    env.storage().persistent().set(&key, &cursor);
    bump_entry_ttl(env, &key);
}

pub fn clear_artist_cancel_cursor(env: &Env, artist: &Address) {
    env.storage()
        .persistent()
        .remove(&DataKey::ArtistCancelCursor(artist.clone()));
}

// ── Moderation & Config ────────────────────────────────────

pub fn set_artist_revocation_storage(env: &Env, artist: &Address) {
    let key = DataKey::RevokedArtist(artist.clone());
    env.storage().persistent().set(&key, &true);
    bump_entry_ttl(env, &key);
}

pub fn remove_artist_revocation_storage(env: &Env, artist: &Address) {
    env.storage()
        .persistent()
        .remove(&DataKey::RevokedArtist(artist.clone()));
}

pub fn is_artist_revoked_storage(env: &Env, artist: &Address) -> bool {
    let key = DataKey::RevokedArtist(artist.clone());
    let revoked = env
        .storage()
        .persistent()
        .get::<_, bool>(&key)
        .unwrap_or(false);
    if revoked {
        bump_entry_ttl(env, &key);
    }
    revoked
}

pub fn set_treasury_storage(env: &Env, addr: &Address) {
    env.storage().persistent().set(&DataKey::Treasury, addr);
    bump_entry_ttl(env, &DataKey::Treasury);
}

pub fn get_treasury_storage(env: &Env) -> Option<Address> {
    let value = env.storage().persistent().get(&DataKey::Treasury);
    if value.is_some() {
        bump_entry_ttl(env, &DataKey::Treasury);
    }
    value
}

pub fn set_protocol_fee_bps_storage(env: &Env, bps: u32) {
    env.storage()
        .persistent()
        .set(&DataKey::ProtocolFeeBps, &bps);
    bump_entry_ttl(env, &DataKey::ProtocolFeeBps);
}

pub fn get_protocol_fee_bps_storage(env: &Env) -> Option<u32> {
    let value = env.storage().persistent().get(&DataKey::ProtocolFeeBps);
    if value.is_some() {
        bump_entry_ttl(env, &DataKey::ProtocolFeeBps);
    }
    value
}

pub fn set_min_bid_increment_storage(env: &Env, increment: i128) {
    env.storage()
        .persistent()
        .set(&DataKey::MinBidIncrement, &increment);
    bump_entry_ttl(env, &DataKey::MinBidIncrement);
}

pub fn get_min_bid_increment_storage(env: &Env) -> Option<i128> {
    let value = env.storage().persistent().get(&DataKey::MinBidIncrement);
    if value.is_some() {
        bump_entry_ttl(env, &DataKey::MinBidIncrement);
    }
    value
}

pub fn set_auction_extension_window_storage(env: &Env, window: u64) {
    env.storage()
        .persistent()
        .set(&DataKey::AuctionExtensionWindow, &window);
    bump_entry_ttl(env, &DataKey::AuctionExtensionWindow);
}

pub fn get_auction_extension_window_storage(env: &Env) -> Option<u64> {
    let value = env
        .storage()
        .persistent()
        .get(&DataKey::AuctionExtensionWindow);
    if value.is_some() {
        bump_entry_ttl(env, &DataKey::AuctionExtensionWindow);
    }
    value
}

pub fn set_auction_extension_trigger_storage(env: &Env, trigger: u64) {
    env.storage()
        .persistent()
        .set(&DataKey::AuctionExtensionTrigger, &trigger);
    bump_entry_ttl(env, &DataKey::AuctionExtensionTrigger);
}

pub fn get_auction_extension_trigger_storage(env: &Env) -> Option<u64> {
    let value = env
        .storage()
        .persistent()
        .get(&DataKey::AuctionExtensionTrigger);
    if value.is_some() {
        bump_entry_ttl(env, &DataKey::AuctionExtensionTrigger);
    }
    value
}

// ── Reentrancy Guards ────────────────────────────────────────

pub fn acquire_listing_lock(env: &Env, listing_id: u64) -> bool {
    let key = DataKey::ListingLock(listing_id);
    if env.storage().temporary().has(&key) {
        return false;
    }
    env.storage().temporary().set(&key, &true);
    env.storage()
        .temporary()
        .extend_ttl(&key, REENTRANCY_LOCK_TTL, REENTRANCY_LOCK_TTL);
    true
}

pub fn release_listing_lock(env: &Env, listing_id: u64) {
    let key = DataKey::ListingLock(listing_id);
    env.storage().temporary().remove(&key);
}

pub fn acquire_auction_lock(env: &Env, auction_id: u64) -> bool {
    let key = DataKey::AuctionLock(auction_id);
    if env.storage().temporary().has(&key) {
        return false;
    }
    env.storage().temporary().set(&key, &true);
    env.storage()
        .temporary()
        .extend_ttl(&key, REENTRANCY_LOCK_TTL, REENTRANCY_LOCK_TTL);
    true
}

pub fn release_auction_lock(env: &Env, auction_id: u64) {
    let key = DataKey::AuctionLock(auction_id);
    env.storage().temporary().remove(&key);
}

// ── Admin transfer ───────────────────────────────────────────

pub fn set_pending_admin_storage(env: &Env, pending: &Address) {
    env.storage()
        .persistent()
        .set(&DataKey::PendingAdmin, pending);
    bump_entry_ttl(env, &DataKey::PendingAdmin);
}

pub fn get_pending_admin_storage(env: &Env) -> Option<Address> {
    let value = env.storage().persistent().get(&DataKey::PendingAdmin);
    if value.is_some() {
        bump_entry_ttl(env, &DataKey::PendingAdmin);
    }
    value
}

pub fn clear_pending_admin_storage(env: &Env) {
    env.storage().persistent().remove(&DataKey::PendingAdmin);
}

// ── Bid history ──────────────────────────────────────────────

pub fn append_bid_record(env: &Env, auction_id: u64, record: &BidRecord, cap: u32) {
    let key = DataKey::AuctionBids(auction_id);
    let mut history = env
        .storage()
        .persistent()
        .get::<DataKey, soroban_sdk::Vec<BidRecord>>(&key)
        .unwrap_or_else(|| soroban_sdk::Vec::new(env));
    if history.len() >= cap {
        let mut trimmed = soroban_sdk::Vec::new(env);
        for i in 1..history.len() {
            trimmed.push_back(history.get(i).unwrap());
        }
        history = trimmed;
    }
    history.push_back(record.clone());
    env.storage().persistent().set(&key, &history);
    bump_entry_ttl(env, &key);
}

pub fn load_auction_bids(env: &Env, auction_id: u64) -> soroban_sdk::Vec<BidRecord> {
    let key = DataKey::AuctionBids(auction_id);
    let value = env
        .storage()
        .persistent()
        .get::<DataKey, soroban_sdk::Vec<BidRecord>>(&key)
        .unwrap_or_else(|| soroban_sdk::Vec::new(env));
    if !value.is_empty() {
        bump_entry_ttl(env, &key);
    }
    value
}

pub fn set_paused(env: &Env, paused: bool) {
    env.storage().persistent().set(&DataKey::IsPaused, &paused);
    bump_entry_ttl(env, &DataKey::IsPaused);
}

pub fn is_paused(env: &Env) -> bool {
    env.storage()
        .persistent()
        .get::<DataKey, bool>(&DataKey::IsPaused)
        .unwrap_or(false)
}

// ── Price bounds ─────────────────────────────────────────────

pub fn set_min_price_storage(env: &Env, min: i128) {
    env.storage().persistent().set(&DataKey::MinPrice, &min);
    bump_entry_ttl(env, &DataKey::MinPrice);
}

pub fn get_min_price_storage(env: &Env) -> Option<i128> {
    let value = env.storage().persistent().get(&DataKey::MinPrice);
    if value.is_some() {
        bump_entry_ttl(env, &DataKey::MinPrice);
    }
    value
}

pub fn set_max_price_storage(env: &Env, max: i128) {
    env.storage().persistent().set(&DataKey::MaxPrice, &max);
    bump_entry_ttl(env, &DataKey::MaxPrice);
}

pub fn get_max_price_storage(env: &Env) -> Option<i128> {
    let value = env.storage().persistent().get(&DataKey::MaxPrice);
    if value.is_some() {
        bump_entry_ttl(env, &DataKey::MaxPrice);
    }
    value
}

// ── Migration marker ─────────────────────────────────────────

pub fn set_migration_done(env: &Env, version: &soroban_sdk::String) {
    let key = DataKey::MigrationDone(version.clone());
    env.storage().persistent().set(&key, &true);
    bump_entry_ttl(env, &key);
}

pub fn is_migration_done(env: &Env, version: &soroban_sdk::String) -> bool {
    let key = DataKey::MigrationDone(version.clone());
    let done = env
        .storage()
        .persistent()
        .get::<_, bool>(&key)
        .unwrap_or(false);
    if done {
        bump_entry_ttl(env, &key);
    }
    done
}

/// Load the resumable migration progress for `version` (phase 0, cursor 0
/// when the migration has not started yet).
pub fn get_migration_progress(env: &Env, version: &soroban_sdk::String) -> MigrationProgress {
    env.storage()
        .persistent()
        .get::<DataKey, MigrationProgress>(&DataKey::MigrationCursor(version.clone()))
        .unwrap_or(MigrationProgress { phase: 0, cursor: 0 })
}

pub fn set_migration_progress(env: &Env, version: &soroban_sdk::String, progress: &MigrationProgress) {
    let key = DataKey::MigrationCursor(version.clone());
    env.storage().persistent().set(&key, progress);
    bump_entry_ttl(env, &key);
}

pub fn clear_migration_progress(env: &Env, version: &soroban_sdk::String) {
    env.storage()
        .persistent()
        .remove(&DataKey::MigrationCursor(version.clone()));
}

/// Read-and-delete a legacy (pre-1.1.0) monolithic `Vec<u64>` index entry.
/// Returns `None` when the key does not exist (already migrated or never
/// written).  Used exclusively by the 1.1.0 storage migration.
pub fn take_legacy_index_vec(env: &Env, key: &DataKey) -> Option<Vec<u64>> {
    let value = env.storage().persistent().get::<DataKey, Vec<u64>>(key);
    if value.is_some() {
        env.storage().persistent().remove(key);
    }
    value
}

// ── Global bid-history cap ───────────────────────────────────

/// Default bid history cap used when the admin has not configured a custom value.
pub const DEFAULT_BID_HISTORY_CAP: u32 = 50;

/// Hard upper-bound for the admin-configurable cap.  Keeps the per-auction
/// Vec bounded and the O(n) eviction shift economically acceptable.
pub const MAX_BID_HISTORY_CAP: u32 = 200;

/// Persist the admin-chosen global bid history cap.
pub fn set_bid_history_cap_storage(env: &Env, cap: u32) {
    env.storage()
        .persistent()
        .set(&DataKey::BidHistoryCap, &cap);
    bump_entry_ttl(env, &DataKey::BidHistoryCap);
}

/// Read the current global bid history cap.  Returns `DEFAULT_BID_HISTORY_CAP`
/// when no custom value has been set yet.
pub fn get_bid_history_cap_storage(env: &Env) -> u32 {
    let value = env
        .storage()
        .persistent()
        .get::<DataKey, u32>(&DataKey::BidHistoryCap);
    if value.is_some() {
        bump_entry_ttl(env, &DataKey::BidHistoryCap);
    }
    value.unwrap_or(DEFAULT_BID_HISTORY_CAP)
}
