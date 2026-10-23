use std::{collections::HashSet, hash::Hash, sync::atomic::Ordering};

use itertools::Itertools;
use jito_protos::shredstream::TraceShred;
use prost::Message;
use solana_clock::{Slot, MAX_PROCESSING_AGE};
use solana_entry::{block_component::BlockComponent, entry::Entry};
use solana_ledger::shred::{
    ReedSolomonCache, Shred, ShredType, Shredder, MAX_DATA_SHREDS_PER_SLOT,
};
use solana_metrics::datapoint_warn;
use solana_perf::packet::PacketBatch;
use tracing::{debug, warn};

use crate::forwarder::ShredMetrics;

#[derive(Default, Debug, Copy, Clone, Eq, PartialEq)]
enum ShredStatus {
    #[default]
    Unknown,
    /// Shred that is **not** marked as [ShredFlags::DATA_COMPLETE_SHRED]
    NotDataComplete,
    /// Shred that is marked as [ShredFlags::DATA_COMPLETE_SHRED]
    DataComplete,
}

/// Tracks per-slot shred information for data shreds
/// Guaranteed to have MAX_DATA_SHREDS_PER_SLOT entries in each Vec
#[derive(Debug)]
pub struct ShredsStateTracker {
    /// Compact status of each data shred for fast iteration.
    data_status: Vec<ShredStatus>,
    /// Data shreds received for the slot (not coding!)
    data_shreds: Vec<Option<Shred>>,
    /// array of bools that track which FEC set indexes have been already recovered
    already_recovered_fec_sets: Vec<bool>,
    /// array of bools that track which data shred indexes have been already deshredded
    already_deshredded: Vec<bool>,
}
impl Default for ShredsStateTracker {
    fn default() -> Self {
        Self {
            data_status: vec![ShredStatus::Unknown; MAX_DATA_SHREDS_PER_SLOT],
            data_shreds: vec![None; MAX_DATA_SHREDS_PER_SLOT],
            already_recovered_fec_sets: vec![false; MAX_DATA_SHREDS_PER_SLOT],
            already_deshredded: vec![false; MAX_DATA_SHREDS_PER_SLOT],
        }
    }
}

/// Returns the number of shreds reconstructed
/// Updates all_shreds with current state, and deshredded_entries with returned values
/// receive shreds per FEC set, attempting to recover the other shreds in the fec set so you do not have to wait until all data shreds have arrived.
/// every time a fec is recovered, scan for neighbouring DATA_COMPLETE_SHRED flags in the shreds, attempting to deserialize into solana entries when there are no missing shreds between the DATA_COMPLETE_SHRED flags.
/// note that an FEC set doesn't necessarily contain DATA_COMPLETE_SHRED in the last shred. when deserializing the bincode data, you must use data between shreds starting at the last DATA_COMPLETE_SHRED (not inclusive) to the next DATA_COMPLETE_SHRED (inclusive)
pub fn reconstruct_shreds(
    packet_batch: PacketBatch,
    all_shreds: &mut ahash::HashMap<
        Slot,
        (
            ahash::HashMap<u32 /* fec_set_index */, HashSet<ComparableShred>>,
            ShredsStateTracker,
        ),
    >,
    slot_fec_indexes_to_iterate: &mut Vec<(Slot, u32)>,
    deshredded_entries: &mut Vec<(Slot, Vec<Entry>, Vec<u8>)>,
    highest_slot_seen: &mut Slot,
    rs_cache: &ReedSolomonCache,
    metrics: &ShredMetrics,
) -> usize {
    deshredded_entries.clear();
    slot_fec_indexes_to_iterate.clear();
    // ingest all packets
    for packet in packet_batch.iter().filter_map(|p| p.data(..)) {
        match Shred::new_from_serialized_shred(packet.to_vec()) {
            Ok(shred) => {
                let slot = shred.slot();
                let index = shred.index() as usize;
                let fec_set_index = shred.fec_set_index();
                let (all_shreds, state_tracker) = all_shreds.entry(slot).or_default();
                if highest_slot_seen.saturating_sub(SLOT_LOOKBACK) > slot {
                    debug!(slot, fec_set_index, index, "old shred");
                    continue;
                }
                if state_tracker.already_recovered_fec_sets[fec_set_index as usize]
                    || state_tracker.already_deshredded[index]
                {
                    debug!(
                        slot,
                        fec_set_index, index, "shred for an already completed FEC set"
                    );
                    continue;
                }
                let Some(_shred_index) = update_state_tracker(&shred, state_tracker) else {
                    continue;
                };

                all_shreds
                    .entry(fec_set_index)
                    .or_default()
                    .insert(ComparableShred(shred));
                slot_fec_indexes_to_iterate.push((slot, fec_set_index)); // use Vec so we can sort to make sure if any earlier FEC sets have DATA_SHRED_COMPLETE, later entries can use the flag to find the bounds
                *highest_slot_seen = std::cmp::max(*highest_slot_seen, slot);
            }
            Err(e) => {
                if TraceShred::decode(packet).is_ok() {
                    continue;
                }
                warn!(bytes = packet.len(), "failed to decode shred: {e}");
            }
        }
    }
    slot_fec_indexes_to_iterate.sort_unstable();
    slot_fec_indexes_to_iterate.dedup();

    // try recovering by FEC set
    // already checked if FEC set is completed or deserialized
    let mut total_recovered_count = 0;
    for (slot, fec_set_index) in slot_fec_indexes_to_iterate.iter() {
        let (all_shreds, state_tracker) = all_shreds.entry(*slot).or_default();
        let shreds = all_shreds.entry(*fec_set_index).or_default();
        let (
            num_expected_data_shreds,
            num_expected_coding_shreds,
            num_data_shreds,
            num_coding_shreds,
        ) = get_data_shred_info(shreds);

        // haven't received last data shred, haven't seen any coding shreds, so wait until more arrive
        let min_shreds_needed_to_recover = num_expected_data_shreds as usize;
        if num_expected_data_shreds == 0
            || shreds.len() < min_shreds_needed_to_recover
            || num_data_shreds == num_expected_data_shreds
        {
            continue;
        }

        // try to recover if we have enough shreds in the FEC set
        let merkle_shreds = shreds
            .iter()
            .sorted_by_key(|s| (u8::MAX - s.shred_type() as u8, s.index()))
            .map(|s| s.0.clone())
            .collect_vec();
        let recovered = match solana_ledger::shred::recover(merkle_shreds, rs_cache) {
            Ok(r) => r, // data shreds followed by code shreds (whatever was missing from to_deshred_payload)
            Err(e) => {
                warn!(
                    slot,
                    fec_set_index,
                    expected_data = num_expected_data_shreds,
                    data = num_data_shreds,
                    expected_coding = num_expected_coding_shreds,
                    coding = num_coding_shreds,
                    "failed to recover FEC set: {e}"
                );
                continue;
            }
        };

        let mut fec_set_recovered_count = 0;
        for shred in recovered {
            match shred {
                Ok(shred) => {
                    if update_state_tracker(&shred, state_tracker).is_none() {
                        continue; // already seen before in state tracker
                    }
                    // shreds.insert(ComparableShred(shred)); // optional since all data shreds are in state_tracker
                    total_recovered_count += 1;
                    fec_set_recovered_count += 1;
                }
                Err(e) => warn!(slot, fec_set_index, "recovered shred is invalid: {e}"),
            }
        }

        if fec_set_recovered_count > 0 {
            debug!(
                slot,
                fec_set_index,
                recovered = fec_set_recovered_count,
                "recovered FEC set"
            );
            state_tracker.already_recovered_fec_sets[*fec_set_index as usize] = true;
            shreds.clear();
        }
    }

    // deshred and deserialize
    for (slot, fec_set_index) in slot_fec_indexes_to_iterate.iter() {
        let (_all_shreds, state_tracker) = all_shreds.entry(*slot).or_default();
        let Some((start_data_complete_idx, end_data_complete_idx, unknown_start)) =
            get_indexes(state_tracker, *fec_set_index as usize)
        else {
            continue;
        };
        if unknown_start {
            metrics
                .unknown_start_position_count
                .fetch_add(1, Ordering::Relaxed);
        }

        let to_deshred =
            &state_tracker.data_shreds[start_data_complete_idx..=end_data_complete_idx];
        let deshredded_payload =
            match Shredder::deshred(to_deshred.iter().map(|s| s.as_ref().unwrap().payload())) {
                Ok(v) => v,
                Err(e) => {
                    if unknown_start {
                        debug!(
                            slot,
                            start = start_data_complete_idx,
                            end = end_data_complete_idx,
                            "guessed batch start failed to deshred: {e}"
                        );
                        metrics
                            .unknown_start_position_error_count
                            .fetch_add(1, Ordering::Relaxed);
                    } else {
                        warn!(
                            slot,
                            start = start_data_complete_idx,
                            end = end_data_complete_idx,
                            "failed to deshred: {e}"
                        );
                        metrics
                            .fec_recovery_error_count
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    continue;
                }
            };

        match wincode::deserialize::<BlockComponent>(&deshredded_payload) {
            Ok(BlockComponent::EntryBatch(entries)) => {
                metrics
                    .entry_count
                    .fetch_add(entries.len() as u64, Ordering::Relaxed);
                let txn_count = entries.iter().map(|e| e.transactions.len() as u64).sum();
                metrics.txn_count.fetch_add(txn_count, Ordering::Relaxed);
                debug!(
                    slot,
                    start = start_data_complete_idx,
                    end = end_data_complete_idx,
                    entries = entries.len(),
                    txns = txn_count,
                    "decoded entry batch"
                );
                deshredded_entries.push((*slot, entries, deshredded_payload));
            }
            Ok(BlockComponent::BlockMarker(_)) => {}
            Err(e) => {
                if unknown_start {
                    debug!(
                        slot,
                        start = start_data_complete_idx,
                        end = end_data_complete_idx,
                        bytes = deshredded_payload.len(),
                        "guessed batch start failed to deserialize: {e}"
                    );
                    metrics
                        .unknown_start_position_error_count
                        .fetch_add(1, Ordering::Relaxed);
                } else {
                    warn!(
                        slot,
                        start = start_data_complete_idx,
                        end = end_data_complete_idx,
                        bytes = deshredded_payload.len(),
                        "failed to deserialize entries: {e}"
                    );
                    metrics
                        .bincode_deserialize_error_count
                        .fetch_add(1, Ordering::Relaxed);
                }
                continue;
            }
        }
        to_deshred.iter().for_each(|shred| {
            let Some(shred) = shred.as_ref() else {
                return;
            };
            state_tracker.already_recovered_fec_sets[shred.fec_set_index() as usize] = true;
            state_tracker.already_deshredded[shred.index() as usize] = true;
        })
    }

    if all_shreds.len() > MAX_PROCESSING_AGE {
        let slot_threshold = highest_slot_seen.saturating_sub(SLOT_LOOKBACK);
        let mut incomplete_fec_sets = ahash::HashMap::<Slot, Vec<_>>::default();
        let mut incomplete_fec_sets_count = 0;
        all_shreds.retain(|slot, (fec_set_indexes, state_tracker)| {
            if *slot >= slot_threshold {
                return true;
            }

            // count missing fec sets before clearing
            for (fec_set_index, shreds) in fec_set_indexes.iter() {
                if state_tracker.already_recovered_fec_sets[*fec_set_index as usize] {
                    continue;
                }
                let (
                    num_expected_data_shreds,
                    _num_expected_coding_shreds,
                    _num_data_shreds,
                    _num_coding_shreds,
                ) = get_data_shred_info(shreds);

                incomplete_fec_sets_count += 1;
                incomplete_fec_sets
                    .entry(*slot)
                    .and_modify(|fec_set_data| {
                        fec_set_data.push((*fec_set_index, num_expected_data_shreds, shreds.len()))
                    })
                    .or_insert_with(|| {
                        vec![(*fec_set_index, num_expected_data_shreds, shreds.len())]
                    });
            }

            false
        });
        if incomplete_fec_sets_count > 0 {
            incomplete_fec_sets
                .iter_mut()
                .for_each(|(_slot, fec_set_indexes)| fec_set_indexes.sort_unstable());
            warn!(
                slots = incomplete_fec_sets.len(),
                fec_sets = incomplete_fec_sets_count,
                "evicted slots with FEC sets that never completed"
            );
            datapoint_warn!(
                "localshred_lite_proxy-deshred_missed_fec_sets",
                (
                    "slot_fec_set_indexes",
                    format!("{:?}", incomplete_fec_sets.iter().sorted().collect_vec()),
                    String
                ),
                ("slot_count", incomplete_fec_sets.len(), i64),
                ("fec_set_count", incomplete_fec_sets_count, i64),
            );
        }
    }

    if total_recovered_count > 0 {
        metrics
            .recovered_count
            .fetch_add(total_recovered_count as u64, Ordering::Relaxed);
    }

    total_recovered_count
}

#[allow(unused)]
fn debug_remaining_shreds(
    all_shreds: &mut ahash::HashMap<
        Slot,
        (
            ahash::HashMap<u32, HashSet<ComparableShred>>,
            ShredsStateTracker,
        ),
    >,
) {
    let mut incomplete_fec_sets = ahash::HashMap::<Slot, Vec<_>>::default();
    let mut incomplete_fec_sets_count = 0;
    all_shreds
        .iter()
        .for_each(|(slot, (fec_set_indexes, state_tracker))| {
            // count missing fec sets before clearing
            for (fec_set_index, shreds) in fec_set_indexes.iter() {
                if state_tracker.already_recovered_fec_sets[*fec_set_index as usize] {
                    continue;
                }
                let (
                    num_expected_data_shreds,
                    _num_expected_coding_shreds,
                    _num_data_shreds,
                    _num_coding_shreds,
                ) = get_data_shred_info(shreds);

                incomplete_fec_sets_count += 1;
                incomplete_fec_sets
                    .entry(*slot)
                    .and_modify(|fec_set_data| {
                        fec_set_data.push((*fec_set_index, num_expected_data_shreds, shreds.len()))
                    })
                    .or_insert_with(|| {
                        vec![(*fec_set_index, num_expected_data_shreds, shreds.len())]
                    });
            }
        });
    incomplete_fec_sets
        .iter_mut()
        .for_each(|(_slot, fec_set_indexes)| fec_set_indexes.sort_unstable());
    println!("{:?}", incomplete_fec_sets.iter().sorted().collect_vec());
}

/// Return the inclusive range of shreds that constitute one complete segment: [0+ NotDataComplete, DataComplete]
/// Rules:
/// * A segment **ends** at the first `DataComplete` *at or after* `index`.
/// * It **starts** one position after the previous `DataComplete`, or at the beginning of the vector if there is none.
/// * If an `Unknown` is seen while searching towards the right, the segment is discarded and `None` is returned.
/// * We allow `Unknown` towards the left since sometimes entire FEC sets are not sent out
fn get_indexes(
    tracker: &ShredsStateTracker,
    index: usize,
) -> Option<(
    usize, /* start_data_complete_idx */
    usize, /* end_data_complete_idx */
    bool,  /* unknown start index */
)> {
    if index >= tracker.data_status.len() {
        return None;
    }

    // find the right boundary (first DataComplete ≥ index)
    let mut end = index;
    while end < tracker.data_status.len() {
        if tracker.already_deshredded[end] {
            return None;
        }
        match &tracker.data_status[end] {
            ShredStatus::Unknown => return None,
            ShredStatus::DataComplete => break,
            ShredStatus::NotDataComplete => end += 1,
        }
    }
    if end == tracker.data_status.len() {
        return None; // never saw a DataComplete
    }

    if end == 0 {
        return Some((0, 0, false)); // the vec *starts* with DataComplete
    }
    if index == 0 {
        return Some((0, end, false));
    }

    // find the left boundary (prev DataComplete + 1)
    let mut start = index;
    let mut next = start - 1;
    loop {
        match tracker.data_status[next] {
            ShredStatus::NotDataComplete => {
                if tracker.already_deshredded[next] {
                    return None; // already covered by some other iteration
                }
                if next == 0 {
                    return Some((0, end, false)); // no earlier DataComplete
                }
                start = next;
                next -= 1;
            }
            ShredStatus::DataComplete => return Some((start, end, false)),
            ShredStatus::Unknown => return Some((start, end, true)), // sometimes we don't have the previous starting shreds, make best guess
        }
    }
}

/// Upon receiving a new shred (either from recovery or receiving a UDP packet), update the state tracker
/// Returns shred index on new insert, None if already exists
fn update_state_tracker(shred: &Shred, state_tracker: &mut ShredsStateTracker) -> Option<usize> {
    let index = shred.index() as usize;
    if state_tracker.already_recovered_fec_sets[shred.fec_set_index() as usize] {
        return None;
    }
    if shred.shred_type() == ShredType::Data
        && (state_tracker.data_shreds[index].is_some()
            || !matches!(state_tracker.data_status[index], ShredStatus::Unknown))
    {
        return None;
    }
    if shred.is_data() {
        state_tracker.data_shreds[index] = Some(shred.clone());
        if shred.data_complete() || shred.last_in_slot() {
            state_tracker.data_status[index] = ShredStatus::DataComplete;
        } else {
            state_tracker.data_status[index] = ShredStatus::NotDataComplete;
        }
    };
    Some(index)
}

const SLOT_LOOKBACK: Slot = 50;
const OFF_CODING_NUM_DATA: usize = 83;
const OFF_CODING_NUM_CODING: usize = 85;

fn read_u16(payload: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([payload[offset], payload[offset + 1]])
}

/// check if we can reconstruct (having minimum number of data + coding shreds)
fn get_data_shred_info(
    shreds: &HashSet<ComparableShred>,
) -> (
    u16, /* num_expected_data_shreds */
    u16, /* num_expected_coding_shreds */
    u16, /* num_data_shreds */
    u16, /* num_coding_shreds */
) {
    let mut num_expected_data_shreds = 0;
    let mut num_expected_coding_shreds = 0;
    let mut num_data_shreds = 0;
    let mut num_coding_shreds = 0;
    for shred in shreds {
        if shred.is_code() {
            num_coding_shreds += 1;
            num_expected_data_shreds = read_u16(shred.payload(), OFF_CODING_NUM_DATA);
            num_expected_coding_shreds = read_u16(shred.payload(), OFF_CODING_NUM_CODING);
        } else {
            num_data_shreds += 1;
            if num_expected_data_shreds == 0 && (shred.data_complete() || shred.last_in_slot()) {
                num_expected_data_shreds = (shred.index() - shred.fec_set_index()) as u16 + 1;
            }
        }
    }
    (
        num_expected_data_shreds,
        num_expected_coding_shreds,
        num_data_shreds,
        num_coding_shreds,
    )
}

#[derive(Clone, Debug, Eq)]
pub struct ComparableShred(Shred);

impl std::ops::Deref for ComparableShred {
    type Target = Shred;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Hash for ComparableShred {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.id().hash(state);
    }
}

impl PartialEq for ComparableShred {
    fn eq(&self, other: &Self) -> bool {
        self.0.id() == other.0.id() && !self.0.is_shred_duplicate(&other.0)
    }
}
#[cfg(test)]
mod tests {
    use std::{
        collections::{hash_map::Entry, HashSet},
        io::{Read, Write},
        net::UdpSocket,
        sync::Arc,
    };

    use borsh::BorshDeserialize;
    use itertools::Itertools;
    use rand::Rng;
    use solana_clock::Slot;
    use solana_hash::Hash;
    use solana_keypair::Keypair;
    use solana_ledger::shred::{ProcessShredsStats, ReedSolomonCache, Shred, ShredId, Shredder};
    use solana_perf::packet::{Packet, PacketBatch, RecycledPacketBatch};

    use crate::{
        deshred::{reconstruct_shreds, ComparableShred},
        forwarder::ShredMetrics,
    };

    const V1: &str = "810100010c00000059b9607667efbffd1176cb5dbd1cf157bf46ee8a1bb080d643aee90d2c2bfc1c01030d04eadff4df92f48a9a2c007aceb3f921e2ff230b793d06f45300bb444124342026101ec20328964a32abab136c5405b91f3ae38ee4f64cb6bde879b86838d20000000000000000000000000000000000000000000000000000000000000000960000000080000002020c00000102000000a08601000000000043635aa608621dc078a8d62f1badfb167f8ff463f47eb25167e5202f590e69b1b3e756119223c4e803a0d3c695439ff4709e252f5a375a5ec6b866aa6cb7fa04";
    const V1_TXS_PER_ENTRY: usize = 8;

    fn batch(packets: Vec<Packet>) -> PacketBatch {
        RecycledPacketBatch::new(packets).into()
    }

    fn v1_entries(num_entries: u64) -> Vec<solana_entry::entry::Entry> {
        let v1 = (0..V1.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&V1[i..i + 2], 16).unwrap())
            .collect::<Vec<_>>();
        (0..num_entries)
            .map(|i| solana_entry::entry::Entry {
                num_hashes: i,
                hash: Hash::new_from_array([i as u8; 32]),
                transactions: vec![wincode::deserialize(&v1).unwrap(); V1_TXS_PER_ENTRY],
            })
            .collect()
    }

    /// For serializing packets to disk
    #[derive(borsh::BorshSerialize, borsh::BorshDeserialize, PartialEq, Debug)]
    struct Packets {
        pub packets: Vec<Vec<u8>>,
    }

    #[allow(unused)]
    fn listen_and_write_shreds() -> std::io::Result<()> {
        let socket = UdpSocket::bind("127.0.0.1:5000")?;
        println!("Listening on {}", socket.local_addr()?);

        let mut map = ahash::HashMap::<usize, usize>::default();
        let mut buf = [0u8; 1500];
        let mut vec = Packets {
            packets: Vec::new(),
        };

        let mut i = 0;
        loop {
            i += 1;
            match socket.recv_from(&mut buf) {
                Ok((amt, _src)) => {
                    vec.packets.push(buf[..amt].to_vec());
                    match map.entry(amt) {
                        Entry::Occupied(mut e) => *e.get_mut() += 1,
                        Entry::Vacant(e) => {
                            e.insert(1);
                        }
                    }
                    *map.get_mut(&amt).unwrap_or(&mut 0) += 1;
                }
                Err(e) => {
                    eprintln!("Error receiving data: {}", e);
                }
            }
            if i % 50000 == 0 {
                dbg!(&map);
                // size 1203 are data shreds: https://github.com/jito-foundation/jito-solana/blob/1742826fca975bd6d17daa5693abda861bbd2adf/ledger/src/shred/merkle.rs#L42
                // size 1228 are coding shreds: https://github.com/jito-foundation/jito-solana/blob/1742826fca975bd6d17daa5693abda861bbd2adf/ledger/src/shred/shred_code.rs#L16
                let mut file = std::fs::File::create("serialized_shreds.bin")?;
                file.write_all(&borsh::to_vec(&vec)?)?;
                return Ok(());
            }
        }
    }

    #[test]
    #[ignore = "requires bins/serialized_shreds.bin from upstream git-lfs"]
    fn test_reconstruct_live_shreds() {
        let packets = {
            let mut file = std::fs::File::open("../bins/serialized_shreds.bin").unwrap();
            let mut buffer = Vec::new();
            file.read_to_end(&mut buffer).unwrap();
            Packets::try_from_slice(&buffer).unwrap()
        };
        assert_eq!(packets.packets.len(), 50_000);

        let shreds = packets
            .packets
            .iter()
            .filter_map(|p| Shred::new_from_serialized_shred(p.clone()).ok())
            .collect::<Vec<_>>();
        assert_eq!(shreds.len(), 49989);

        let unique_shreds = packets
            .packets
            .iter()
            .filter_map(|p| {
                Shred::new_from_serialized_shred(p.clone())
                    .ok()
                    .map(ComparableShred)
            })
            .collect::<HashSet<ComparableShred>>();
        assert_eq!(unique_shreds.len(), 44900);

        let unique_slot_fec_shreds = packets
            .packets
            .iter()
            .filter_map(|p| {
                Shred::new_from_serialized_shred(p.clone())
                    .ok()
                    .map(|s| s.id())
            })
            .collect::<HashSet<ShredId>>();
        assert_eq!(unique_slot_fec_shreds.len(), 44900);

        let rs_cache = ReedSolomonCache::default();
        let metrics = Arc::new(ShredMetrics::default());

        // Test 1: all shreds provided
        let mut all_shreds = ahash::HashMap::default();
        let mut slot_fec_indexes_to_iterate: Vec<(Slot, u32)> = Vec::new();
        let mut deshredded_entries = Vec::new();
        let mut highest_slot_seen = 0;
        let recovered_count = reconstruct_shreds(
            batch(
                packets
                    .packets
                    .iter()
                    .map(|x| {
                        let mut packet = Packet::default();
                        packet.buffer_mut()[..x.len()].copy_from_slice(x);
                        packet.meta_mut().size = x.len();
                        packet
                    })
                    .collect_vec(),
            ),
            &mut all_shreds,
            &mut slot_fec_indexes_to_iterate,
            &mut deshredded_entries,
            &mut highest_slot_seen,
            &rs_cache,
            &metrics,
        );

        // debug_to_disk(&mut deshredded_entries);
        assert!(recovered_count < deshredded_entries.len());
        assert_eq!(
            deshredded_entries
                .iter()
                .map(|(_slot, entries, _entries_bytes)| entries.len())
                .sum::<usize>(),
            13580
        );
        assert_eq!(all_shreds.len(), 30);

        let slot_to_entry = deshredded_entries
            .iter()
            .into_group_map_by(|(slot, _entries, _entries_bytes)| *slot);
        // slot_to_entry
        //     .iter()
        //     .sorted_by_key(|(slot, _)| *slot)
        //     .for_each(|(slot, entry)| {
        //         println!(
        //             "slot {slot} entry count: {:?}, txn count: {}",
        //             entry.len(),
        //             entry
        //                 .iter()
        //                 .map(|(_slot, entry)| entry.transactions.len())
        //                 .sum::<usize>()
        //         );
        //     });
        assert_eq!(slot_to_entry.len(), 29);

        // Test 2: 33% of shreds missing
        let mut all_shreds = ahash::HashMap::default();
        let mut slot_fec_indexes_to_iterate: Vec<(Slot, u32)> = Vec::new();
        let mut deshredded_entries = Vec::new();
        let mut highest_slot_seen = 0;
        let recovered_count = reconstruct_shreds(
            batch(
                packets
                    .packets
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| (index + 1) % 3 != 0)
                    .map(|(_i, x)| {
                        let mut packet = Packet::default();
                        packet.buffer_mut()[..x.len()].copy_from_slice(x);
                        packet.meta_mut().size = x.len();
                        packet
                    })
                    .collect_vec(),
            ),
            &mut all_shreds,
            &mut slot_fec_indexes_to_iterate,
            &mut deshredded_entries,
            &mut highest_slot_seen,
            &rs_cache,
            &metrics,
        );

        // debug_to_disk(&deshredded_entries, "new.txt");
        assert!(recovered_count > (deshredded_entries.len() / 4));
        assert_eq!(
            deshredded_entries
                .iter()
                .map(|(_slot, entries, _entries_bytes)| entries.len())
                .sum::<usize>(),
            13580
        );
        assert!(all_shreds.len() > 15);

        let slot_to_entry = deshredded_entries
            .iter()
            .into_group_map_by(|(slot, _entries, _entries_bytes)| *slot);
        assert_eq!(slot_to_entry.len(), 29);
    }

    /// Helper function to compare all shred output
    #[allow(unused)]
    fn debug_to_disk(
        deshredded_entries: &[(Slot, Vec<solana_entry::entry::Entry>, Vec<u8>)],
        filepath: &str,
    ) {
        let entries = deshredded_entries
            .iter()
            .map(|(slot, entries, _entries_bytes)| (slot, entries))
            .into_group_map_by(|(slot, _entries)| *slot)
            .into_iter()
            .map(|(key, values)| {
                (
                    key,
                    values.into_iter().fold(Vec::new(), |mut acc, (_, v)| {
                        acc.extend(v);
                        acc
                    }),
                )
            })
            .map(|(slot, entries)| {
                let mut vec = entries
                    .iter()
                    .flat_map(|x| x.transactions.iter())
                    .map(|x| x.signatures[0])
                    .collect::<Vec<_>>();
                vec.sort();
                vec.dedup();
                (slot, vec)
            })
            .sorted_by_key(|x| x.0)
            .dedup_by(|lhs, rhs| lhs.0 == rhs.0)
            .collect_vec();
        let mut file = std::fs::File::create(filepath).unwrap();
        write!(file, "entries: {:#?}", &entries).unwrap();
    }

    #[test]
    #[ignore = "requires bins/serialized_shreds_data_complete_test.bin from upstream git-lfs"]
    /// Test if DATA_COMPLETE_SHRED across multiple FEC sets is handled correctly
    fn test_reconstruct_live_data_complete_shred() {
        let packets = {
            let mut file =
                std::fs::File::open("../bins/serialized_shreds_data_complete_test.bin").unwrap();
            let mut buffer = Vec::new();
            file.read_to_end(&mut buffer).unwrap();
            Packets::try_from_slice(&buffer).unwrap()
        };
        assert_eq!(packets.packets.len(), 150_000);

        let shreds = packets
            .packets
            .iter()
            .filter_map(|p| Shred::new_from_serialized_shred(p.clone()).ok())
            .collect::<Vec<_>>();
        assert_eq!(shreds.len(), 149977);

        let unique_shreds = packets
            .packets
            .iter()
            .filter_map(|p| {
                Shred::new_from_serialized_shred(p.clone())
                    .ok()
                    .map(ComparableShred)
            })
            .collect::<HashSet<ComparableShred>>();
        assert_eq!(unique_shreds.len(), 109221);

        let unique_slot_fec_shreds = packets
            .packets
            .iter()
            .filter_map(|p| {
                Shred::new_from_serialized_shred(p.clone())
                    .ok()
                    .map(|s| s.id())
            })
            .collect::<HashSet<ShredId>>();
        assert_eq!(unique_slot_fec_shreds.len(), 109221);

        let rs_cache = ReedSolomonCache::default();
        let metrics = Arc::new(ShredMetrics::default());

        // Test 1: all shreds provided
        let mut all_shreds = ahash::HashMap::default();
        let mut slot_fec_indexes_to_iterate: Vec<(Slot, u32)> = Vec::new();
        let mut deshredded_entries = Vec::new();
        let mut highest_slot_seen = 0;
        let recovered_count = reconstruct_shreds(
            batch(
                packets
                    .packets
                    .iter()
                    .map(|x| {
                        let mut packet = Packet::default();
                        packet.buffer_mut()[..x.len()].copy_from_slice(x);
                        packet.meta_mut().size = x.len();
                        packet
                    })
                    .collect_vec(),
            ),
            &mut all_shreds,
            &mut slot_fec_indexes_to_iterate,
            &mut deshredded_entries,
            &mut highest_slot_seen,
            &rs_cache,
            &metrics,
        );

        // debug_to_disk(&mut deshredded_entries);
        assert!(recovered_count < deshredded_entries.len());
        assert_eq!(
            deshredded_entries
                .iter()
                .map(|(_slot, entries, _entries_bytes)| entries.len())
                .sum::<usize>(),
            43170
        );
        assert_eq!(all_shreds.len(), 61);

        let slot_to_entry = deshredded_entries
            .iter()
            .into_group_map_by(|(slot, _entries, _entries_bytes)| *slot);
        // slot_to_entry
        //     .iter()
        //     .sorted_by_key(|(slot, _)| *slot)
        //     .for_each(|(slot, entry)| {
        //         println!(
        //             "slot {slot} entry count: {:?}, txn count: {}",
        //             entry.len(),
        //             entry
        //                 .iter()
        //                 .map(|(_slot, entry)| entry.transactions.len())
        //                 .sum::<usize>()
        //         );
        //     });
        assert_eq!(slot_to_entry.len(), 61);

        // Test 2: 33% of shreds missing
        let mut all_shreds = ahash::HashMap::default();
        let mut slot_fec_indexes_to_iterate: Vec<(Slot, u32)> = Vec::new();
        let mut deshredded_entries = Vec::new();
        let mut highest_slot_seen = 0;
        let recovered_count = reconstruct_shreds(
            batch(
                packets
                    .packets
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| (index + 1) % 3 != 0)
                    .map(|(_i, x)| {
                        let mut packet = Packet::default();
                        packet.buffer_mut()[..x.len()].copy_from_slice(x);
                        packet.meta_mut().size = x.len();
                        packet
                    })
                    .collect_vec(),
            ),
            &mut all_shreds,
            &mut slot_fec_indexes_to_iterate,
            &mut deshredded_entries,
            &mut highest_slot_seen,
            &rs_cache,
            &metrics,
        );

        // debug_to_disk(&deshredded_entries, "new.txt");
        assert!(recovered_count > (deshredded_entries.len() / 4));
        assert_eq!(
            deshredded_entries
                .iter()
                .map(|(_slot, entries, _entries_bytes)| entries.len())
                .sum::<usize>(),
            43170
        );
        assert!(all_shreds.len() > 15);

        let slot_to_entry = deshredded_entries
            .iter()
            .into_group_map_by(|(slot, _entries, _entries_bytes)| *slot);
        assert_eq!(slot_to_entry.len(), 61);
    }

    #[test]
    fn test_recover_shreds() {
        let mut rng = rand::rng();
        let slot = 11_111;
        let leader_keypair = Arc::new(Keypair::new());
        let reed_solomon_cache = ReedSolomonCache::default();
        let shredder = Shredder::new(slot, slot - 1, 0, 0).unwrap();
        let chained_merkle_root = Hash::new_from_array(rng.random());
        let num_entry_groups = 10;
        let num_entries = 10;
        let mut entries = Vec::new();
        let mut data_shreds = Vec::new();
        let mut coding_shreds = Vec::new();

        let mut index = 0;
        (0..num_entry_groups).for_each(|_i| {
            let _entries = v1_entries(num_entries);
            let (_data_shreds, _coding_shreds) = shredder.entries_to_merkle_shreds_for_tests(
                &leader_keypair,
                _entries.as_slice(),
                true,
                chained_merkle_root,
                index as u32, // next_shred_index
                index as u32, // next_code_index,
                &reed_solomon_cache,
                &mut ProcessShredsStats::default(),
            );
            index += _data_shreds.len();
            entries.extend(_entries);
            data_shreds.extend(_data_shreds);
            coding_shreds.extend(_coding_shreds);
        });

        let packets = data_shreds
            .iter()
            .chain(coding_shreds.iter())
            .map(|s| {
                let mut p = Packet::default();
                s.copy_to_packet(&mut p);
                p
            })
            .collect_vec();
        assert_eq!(data_shreds.len(), 320);
        assert_eq!(
            data_shreds
                .iter()
                .map(|s| s.fec_set_index())
                .dedup()
                .count(),
            num_entry_groups
        );

        let metrics = Arc::new(ShredMetrics::default());
        let rs_cache = ReedSolomonCache::default();

        // Test 1: all shreds provided
        let mut all_shreds = ahash::HashMap::default();
        let mut slot_fec_indexes_to_iterate: Vec<(Slot, u32)> = Vec::new();
        let mut deshredded_entries = Vec::new();
        let mut highest_slot_seen = 0;
        let recovered_count = reconstruct_shreds(
            batch(packets.clone()),
            &mut all_shreds,
            &mut slot_fec_indexes_to_iterate,
            &mut deshredded_entries,
            &mut highest_slot_seen,
            &rs_cache,
            &metrics,
        );
        assert_eq!(recovered_count, 0);
        assert_eq!(
            deshredded_entries
                .iter()
                .map(|(_slot, entries, _entries_bytes)| entries.len())
                .sum::<usize>(),
            entries.len()
        );
        assert_eq!(
            all_shreds.len(),
            1, // slot 11111
        );

        // Test 2: 33% of shreds missing
        let mut all_shreds = ahash::HashMap::default();
        let mut slot_fec_indexes_to_iterate: Vec<(Slot, u32)> = Vec::new();
        let mut deshredded_entries = Vec::new();
        let mut highest_slot_seen = 0;
        let recovered_count = reconstruct_shreds(
            batch(
                packets
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| (index + 1) % 3 != 0)
                    .map(|(_i, p)| p.clone())
                    .collect(),
            ),
            &mut all_shreds,
            &mut slot_fec_indexes_to_iterate,
            &mut deshredded_entries,
            &mut highest_slot_seen,
            &rs_cache,
            &metrics,
        );
        assert!(recovered_count > 0);
        assert_eq!(
            deshredded_entries
                .iter()
                .map(|(_slot, entries, _entries_bytes)| entries.len())
                .sum::<usize>(),
            entries.len()
        );
        assert_eq!(
            deshredded_entries
                .iter()
                .flat_map(|(_slot, entries, _entries_bytes)| entries)
                .map(|entry| entry.transactions.len())
                .sum::<usize>(),
            entries.len() * V1_TXS_PER_ENTRY
        );
        assert_eq!(
            all_shreds.len(),
            1, // slot 11111
        );
    }
}
#[cfg(test)]
mod get_indexes_tests {
    use super::{get_indexes, ShredStatus, ShredsStateTracker};

    fn make_test_statustracker(statuses: &[ShredStatus]) -> ShredsStateTracker {
        let mut tracker = ShredsStateTracker::default();
        tracker.data_status[..statuses.len()].copy_from_slice(statuses);
        tracker
    }

    #[test]
    fn start_at_index_zero() {
        let s = [
            ShredStatus::NotDataComplete,
            ShredStatus::NotDataComplete,
            ShredStatus::DataComplete,
        ];
        let tracker = make_test_statustracker(&s);
        assert_eq!(get_indexes(&tracker, 0), Some((0, 2, false)));

        let s = [
            ShredStatus::DataComplete,
            ShredStatus::NotDataComplete,
            ShredStatus::DataComplete,
        ];
        let tracker = make_test_statustracker(&s);
        assert_eq!(get_indexes(&tracker, 0), Some((0, 0, false)));

        let s = [
            ShredStatus::Unknown,
            ShredStatus::NotDataComplete,
            ShredStatus::DataComplete,
        ];
        let tracker = make_test_statustracker(&s);
        assert_eq!(get_indexes(&tracker, 0), None);
    }

    #[test]
    fn start_just_after_data_complete() {
        let s = [
            ShredStatus::DataComplete,
            ShredStatus::NotDataComplete,
            ShredStatus::NotDataComplete,
            ShredStatus::DataComplete,
        ];
        let tracker = make_test_statustracker(&s);
        assert_eq!(get_indexes(&tracker, 1), Some((1, 3, false)));
    }

    #[test]
    fn start_just_before_data_complete() {
        let s = [
            ShredStatus::DataComplete,
            ShredStatus::NotDataComplete,
            ShredStatus::DataComplete,
        ];
        let tracker = make_test_statustracker(&s);
        assert_eq!(get_indexes(&tracker, 1), Some((1, 2, false)));
    }

    #[test]
    fn two_consecutive_data_complete() {
        let s = [
            ShredStatus::NotDataComplete,
            ShredStatus::DataComplete,
            ShredStatus::DataComplete,
        ];
        let tracker = make_test_statustracker(&s);
        assert_eq!(get_indexes(&tracker, 1), Some((0, 1, false)));
        assert_eq!(get_indexes(&tracker, 2), Some((2, 2, false)));
    }

    #[test]
    fn three_consecutive_data_complete() {
        let s = [
            ShredStatus::NotDataComplete,
            ShredStatus::DataComplete,
            ShredStatus::DataComplete,
            ShredStatus::DataComplete,
            ShredStatus::NotDataComplete,
        ];
        let tracker = make_test_statustracker(&s);
        assert_eq!(get_indexes(&tracker, 1), Some((0, 1, false)));
        assert_eq!(get_indexes(&tracker, 2), Some((2, 2, false)));
        assert_eq!(get_indexes(&tracker, 3), Some((3, 3, false)));
    }

    #[test]
    fn unknown_discards_segment() {
        let s = [
            ShredStatus::NotDataComplete,
            ShredStatus::Unknown,
            ShredStatus::DataComplete,
        ];
        let tracker = make_test_statustracker(&s);
        assert_eq!(get_indexes(&tracker, 0), None);

        let s = [
            ShredStatus::Unknown,
            ShredStatus::NotDataComplete,
            ShredStatus::DataComplete,
        ];
        let tracker = make_test_statustracker(&s);
        assert_eq!(get_indexes(&tracker, 1), Some((1, 2, true)));
    }

    #[test]
    fn test_unknown() {
        let s = [
            ShredStatus::Unknown,
            ShredStatus::DataComplete,
            ShredStatus::DataComplete,
            ShredStatus::NotDataComplete,
            ShredStatus::DataComplete,
        ];
        let tracker = make_test_statustracker(&s);
        assert_eq!(get_indexes(&tracker, 0), None);
        assert_eq!(get_indexes(&tracker, 1), Some((1, 1, true)));
        assert_eq!(get_indexes(&tracker, 2), Some((2, 2, false)));
        assert_eq!(get_indexes(&tracker, 3), Some((3, 4, false)));
    }
}
