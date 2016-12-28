// Copyright 2015 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under (1) the MaidSafe.net Commercial License,
// version 1.0 or later, or (2) The General Public License (GPL), version 3, depending on which
// licence you accepted on initial access to the Software (the "Licences").
//
// By contributing code to the SAFE Network Software, or to this project generally, you agree to be
// bound by the terms of the MaidSafe Contributor Agreement, version 1.0.  This, along with the
// Licenses can be found in the root directory of this project at LICENSE, COPYING and CONTRIBUTOR.
//
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.
//
// Please review the Licences for the specific language governing permissions and limitations
// relating to use of the SAFE Network Software.


use ::GROUP_SIZE;
use accumulator::Accumulator;
use chunk_store::ChunkStore;
use error::InternalError;
use itertools::Itertools;
use maidsafe_utilities::{self, serialisation};
use routing::{AppendWrapper, Authority, Data, DataIdentifier, MessageId, RoutingTable,
              StructuredData, XorName};
use routing::client_errors::{GetError, MutationError};
use std::collections::{HashMap, HashSet, VecDeque};
use std::convert::From;
use std::fmt::{self, Debug, Formatter};
use std::ops::Add;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use vault::RoutingNode;

const MAX_FULL_PERCENT: u64 = 50;
/// The quorum for accumulating refresh messages.
const ACCUMULATOR_QUORUM: usize = GROUP_SIZE / 2 + 1;
/// The timeout for accumulating refresh messages.
const ACCUMULATOR_TIMEOUT_SECS: u64 = 180;
/// The timeout for cached data from requests; if no consensus is reached, the data is dropped.
const PENDING_WRITE_TIMEOUT_SECS: u64 = 60;
/// The timeout for retrieving data chunks from individual peers.
const GET_FROM_DATA_HOLDER_TIMEOUT_SECS: u64 = 60;
/// The interval for print status log.
const STATUS_LOG_INTERVAL: u64 = 120;

/// Specification of a particular version of a data chunk. For immutable data, the `u64` is always
/// 0; for structured and appendable data, it specifies the version.
pub type IdAndVersion = (DataIdentifier, u64);

/// A pending write to the chunk store. This is cached in memory until the group either reaches
/// consensus and stores the chunk, or it times out and is dropped.
struct PendingWrite {
    hash: u64,
    data: Data,
    timestamp: Instant,
    src: Authority<XorName>,
    dst: Authority<XorName>,
    message_id: MessageId,
    mutate_type: PendingMutationType,
    rejected: bool,
}

#[derive(Clone, RustcEncodable)]
enum PendingMutationType {
    Append,
    Put,
    Post,
    Delete,
}

struct Cache {
    /// Chunks we are no longer responsible for. These can be deleted from the chunk store.
    unneeded_chunks: VecDeque<DataIdentifier>,
    /// Maps the peers to the set of data chunks that we need and we know they hold.
    data_holders: HashMap<XorName, HashSet<IdAndVersion>>,
    /// Maps the peers to the data chunks we requested from them, and the timestamp of the request.
    ongoing_gets: HashMap<XorName, (Instant, IdAndVersion)>,
    ongoing_gets_count: usize,
    data_holder_items_count: usize,
    logging_time: Instant,
    /// Maps data identifiers to the list of pending writes that affect that chunk.
    pending_writes: HashMap<DataIdentifier, Vec<PendingWrite>>,
}

impl Default for Cache {
    fn default() -> Cache {
        Cache {
            unneeded_chunks: VecDeque::new(),
            data_holders: HashMap::new(),
            ongoing_gets: HashMap::new(),
            ongoing_gets_count: 0,
            data_holder_items_count: 0,
            logging_time: Instant::now(),
            pending_writes: HashMap::new(),
        }
    }
}

impl Cache {
    fn insert_into_ongoing_gets(&mut self, idle_holder: &XorName, data_idv: &IdAndVersion) {
        let _ = self.ongoing_gets.insert(*idle_holder, (Instant::now(), *data_idv));
    }

    fn handle_get_success(&mut self, src: XorName, data_id: &DataIdentifier, version: u64) {
        if let Some((timestamp, expected_idv)) = self.ongoing_gets.remove(&src) {
            if expected_idv.0 != *data_id {
                let _ = self.ongoing_gets.insert(src, (timestamp, expected_idv));
            }
        }
        for (_, data_idvs) in &mut self.data_holders {
            let _ = data_idvs.remove(&(*data_id, version));
        }
    }

    fn handle_get_failure(&mut self, src: XorName, data_id: &DataIdentifier) -> bool {
        if let Some((timestamp, data_idv)) = self.ongoing_gets.remove(&src) {
            if data_idv.0 == *data_id {
                return true;
            } else {
                let _ = self.ongoing_gets.insert(src, (timestamp, data_idv));
            }
        };
        false
    }

    fn register_data_with_holder(&mut self, src: &XorName, data_idv: &IdAndVersion) -> bool {
        if self.data_holders.values().any(|data_idvs| data_idvs.contains(data_idv)) {
            let _ = self.data_holders.entry(*src).or_insert_with(HashSet::new).insert(*data_idv);
            return true;
        }
        false
    }

    fn add_records(&mut self, data_idv: IdAndVersion, holders: HashSet<XorName>) {
        for holder in holders {
            let _ = self.data_holders.entry(holder).or_insert_with(HashSet::new).insert(data_idv);
        }
    }

    fn is_in_unneeded(&self, data_id: &DataIdentifier) -> bool {
        self.unneeded_chunks.iter().any(|id| id == data_id)
    }

    fn add_as_unneeded(&mut self, data_id: DataIdentifier) {
        self.unneeded_chunks.push_back(data_id);
    }

    fn chain_records_in_cache<I>(&self, records_in_store: I) -> HashSet<IdAndVersion>
        where I: IntoIterator<Item = IdAndVersion>
    {
        let mut records: HashSet<_> = self.data_holders
            .values()
            .flat_map(|idvs| idvs.iter().cloned())
            .chain(self.ongoing_gets.values().map(|&(_, idv)| idv))
            .chain(records_in_store)
            .collect();
        for data_id in &self.unneeded_chunks {
            let _ = records.remove(&(*data_id, 0));
        }
        records
    }

    fn prune_unneeded_chunks(&mut self, routing_table: &RoutingTable<XorName>) -> u64 {
        let pruned_unneeded_chunks: HashSet<_> = self.unneeded_chunks
            .iter()
            .filter(|data_id| routing_table.is_closest(data_id.name(), GROUP_SIZE))
            .cloned()
            .collect();
        if !pruned_unneeded_chunks.is_empty() {
            self.unneeded_chunks.retain(|data_id| !pruned_unneeded_chunks.contains(data_id));
        }
        pruned_unneeded_chunks.len() as u64
    }

    fn pop_unneeded_chunk(&mut self) -> Option<DataIdentifier> {
        self.unneeded_chunks.pop_front()
    }

    /// Removes entries from `data_holders` that are no longer valid due to churn.
    fn prune_data_holders(&mut self, routing_table: &RoutingTable<XorName>) {
        let mut empty_holders = Vec::new();
        for (holder, data_idvs) in &mut self.data_holders {
            let lost_idvs = data_idvs.iter()
                .filter(|&&(ref data_id, _)| {
                    // The data needs to be removed if either we are not close to it anymore, i. e.
                    // other_closest_names returns None, or `holder` is not in it anymore.
                    routing_table.other_closest_names(data_id.name(), GROUP_SIZE)
                        .map_or(true, |group| !group.contains(&holder))
                })
                .cloned()
                .collect_vec();
            for lost_idv in lost_idvs {
                let _ = data_idvs.remove(&lost_idv);
            }
            if data_idvs.is_empty() {
                empty_holders.push(*holder);
            }
        }
        for holder in empty_holders {
            let _ = self.data_holders.remove(&holder);
        }
    }

    /// Remove entries from `ongoing_gets` that are no longer responsible for the data or that
    /// disconnected.
    fn prune_ongoing_gets(&mut self, routing_table: &RoutingTable<XorName>) -> bool {
        let lost_gets = self.ongoing_gets
            .iter()
            .filter(|&(holder, &(_, (ref data_id, _)))| {
                routing_table.other_closest_names(data_id.name(), GROUP_SIZE)
                    .map_or(true, |group| !group.contains(&holder))
            })
            .map(|(holder, _)| *holder)
            .collect_vec();
        if !lost_gets.is_empty() {
            for holder in lost_gets {
                let _ = self.ongoing_gets.remove(&holder);
            }
            return true;
        }
        false
    }

    fn needed_data(&mut self) -> Vec<(XorName, IdAndVersion)> {
        let empty_holders = self.data_holders
            .iter()
            .filter(|&(_, data_idvs)| data_idvs.is_empty())
            .map(|(holder, _)| *holder)
            .collect_vec();
        for holder in empty_holders {
            let _ = self.data_holders.remove(&holder);
        }
        let expired_gets = self.ongoing_gets
            .iter()
            .filter(|&(_, &(ref timestamp, _))| {
                timestamp.elapsed().as_secs() > GET_FROM_DATA_HOLDER_TIMEOUT_SECS
            })
            .map(|(holder, _)| *holder)
            .collect_vec();
        for holder in expired_gets {
            let _ = self.ongoing_gets.remove(&holder);
        }
        let mut outstanding_data_ids: HashSet<_> = self.ongoing_gets
            .values()
            .map(|&(_, (data_id, _))| data_id)
            .collect();
        let idle_holders = self.data_holders
            .keys()
            .filter(|holder| !self.ongoing_gets.contains_key(holder))
            .cloned()
            .collect_vec();
        let mut candidates = Vec::new();
        for idle_holder in idle_holders {
            if let Some(data_idvs) = self.data_holders.get_mut(&idle_holder) {
                if let Some(&data_idv) = data_idvs.iter()
                    .find(|&&(ref data_id, _)| !outstanding_data_ids.contains(data_id)) {
                    let _ = data_idvs.remove(&data_idv);
                    let (data_id, _) = data_idv;
                    let _ = outstanding_data_ids.insert(data_id);
                    candidates.push((idle_holder, data_idv));
                }
            }
        }
        candidates
    }

    fn print_stats(&mut self) {
        if self.logging_time.elapsed().as_secs() < STATUS_LOG_INTERVAL {
            return;
        }
        self.logging_time = Instant::now();
        let new_og_count = self.ongoing_gets.len();
        let new_dhi_count = self.data_holders.values().map(HashSet::len).fold(0, Add::add);
        if new_og_count != self.ongoing_gets_count ||
           new_dhi_count != self.data_holder_items_count {
            self.ongoing_gets_count = new_og_count;
            self.data_holder_items_count = new_dhi_count;
            info!("Cache Stats - Expecting {} Get responses. {} entries in data_holders.",
                  new_og_count,
                  new_dhi_count);
        }
    }

    /// Removes and returns all timed out pending writes.
    fn remove_expired_writes(&mut self) -> Vec<PendingWrite> {
        let timeout = Duration::from_secs(PENDING_WRITE_TIMEOUT_SECS);
        let expired_writes = self.pending_writes
            .iter_mut()
            .flat_map(|(_, writes)| {
                writes.iter()
                    .position(|write| write.timestamp.elapsed() > timeout)
                    .map_or_else(Vec::new, |index| writes.split_off(index))
                    .into_iter()
            })
            .collect_vec();
        let expired_keys = self.pending_writes
            .iter_mut()
            .filter(|entry| entry.1.is_empty())
            .map(|(data_id, _)| *data_id)
            .collect_vec();
        for data_id in expired_keys {
            let _ = self.pending_writes.remove(&data_id);
        }
        expired_writes
    }

    /// Inserts the given data as a pending write to the chunk store. If it is the first for that
    /// data identifier, it returns a refresh message to send to ourselves as a group.
    fn insert_pending_write(&mut self,
                            data: Data,
                            mutate_type: PendingMutationType,
                            src: Authority<XorName>,
                            dst: Authority<XorName>,
                            msg_id: MessageId,
                            rejected: bool)
                            -> Option<RefreshData> {
        let hash_pair = match serialisation::serialise(&(data.clone(), mutate_type.clone())) {
            Err(_) => return None,
            Ok(serialised) => serialised,
        };
        let hash = maidsafe_utilities::big_endian_sip_hash(&hash_pair);
        let (data_id, version) = id_and_version_of(&data);

        let pending_write = PendingWrite {
            hash: hash,
            data: data,
            timestamp: Instant::now(),
            src: src,
            dst: dst,
            message_id: msg_id,
            mutate_type: mutate_type,
            rejected: rejected,
        };
        let mut writes = self.pending_writes.entry(data_id).or_insert_with(Vec::new);
        let result = if !rejected && writes.iter().all(|pending_write| pending_write.rejected) {
            Some(RefreshData((data_id, version), hash))
        } else {
            None
        };
        writes.insert(0, pending_write);
        result
    }

    /// Removes and returns all pending writes for the specified data identifier from the cache.
    fn take_pending_writes(&mut self, data_id: &DataIdentifier) -> Vec<PendingWrite> {
        self.pending_writes.remove(data_id).unwrap_or_else(Vec::new)
    }
}


pub struct DataManager {
    chunk_store: ChunkStore<DataIdentifier, Data>,
    /// Accumulates refresh messages and the peers we received them from.
    refresh_accumulator: Accumulator<IdAndVersion, XorName>,
    cache: Cache,
    immutable_data_count: u64,
    structured_data_count: u64,
    appendable_data_count: u64,
    client_get_requests: u64,
    logging_time: Instant,
}

fn id_and_version_of(data: &Data) -> IdAndVersion {
    (data.identifier(),
     match *data {
         Data::Structured(ref sd) => sd.get_version(),
         Data::PubAppendable(ref ad) => ad.get_version(),
         Data::PrivAppendable(ref ad) => ad.get_version(),
         Data::Immutable(_) => 0,
     })
}

impl Debug for DataManager {
    fn fmt(&self, formatter: &mut Formatter) -> fmt::Result {
        write!(formatter,
               "Stats : Client Get requests received {} ; Data stored - ID {} - SD {} - AD {} - \
               total {} bytes",
               self.client_get_requests,
               self.immutable_data_count,
               self.structured_data_count,
               self.appendable_data_count,
               self.chunk_store.used_space())
    }
}

impl DataManager {
    pub fn new(chunk_store_root: PathBuf,
               capacity: u64)
               -> Result<DataManager, InternalError> {
        Ok(DataManager {
            chunk_store: ChunkStore::new(chunk_store_root, capacity)?,
            refresh_accumulator:
                Accumulator::with_duration(ACCUMULATOR_QUORUM,
                                           Duration::from_secs(ACCUMULATOR_TIMEOUT_SECS)),
            cache: Default::default(),
            immutable_data_count: 0,
            structured_data_count: 0,
            appendable_data_count: 0,
            client_get_requests: 0,
            logging_time: Instant::now(),
        })
    }

    pub fn handle_get(&mut self,
                      routing_node: &mut RoutingNode,
                      src: Authority<XorName>,
                      dst: Authority<XorName>,
                      data_id: DataIdentifier,
                      message_id: MessageId)
                      -> Result<(), InternalError> {
        if let Authority::Client { .. } = src {
            self.client_get_requests += 1;
            if self.logging_time.elapsed().as_secs() > STATUS_LOG_INTERVAL {
                self.logging_time = Instant::now();
                info!("{:?}", self);
            }
        }
        if let Ok(data) = self.chunk_store.get(&data_id) {
            trace!("As {:?} sending data {:?} to {:?}", dst, data, src);
            let _ = routing_node.send_get_success(dst, src, data, message_id);
            return Ok(());
        }
        trace!("DM sending get_failure of {:?}", data_id);
        let error = GetError::NoSuchData;
        let external_error_indicator = serialisation::serialise(&error)?;
        routing_node
            .send_get_failure(dst, src, data_id, external_error_indicator, message_id)?;
        Ok(())
    }

    pub fn handle_put(&mut self,
                      routing_node: &mut RoutingNode,
                      src: Authority<XorName>,
                      dst: Authority<XorName>,
                      data: Data,
                      message_id: MessageId)
                      -> Result<(), InternalError> {
        let data_id = data.identifier();
        let mut valid = true;

        if self.chunk_store.has(&data_id) {
            match data_id {
                DataIdentifier::PubAppendable(..) |
                DataIdentifier::PrivAppendable(..) |
                DataIdentifier::Structured(..) => {
                    let old_data_result = self.chunk_store.get(&data_id);
                    valid = match (old_data_result, &data) {
                        (Ok(Data::Structured(ref old_data)), &Data::Structured(ref new_data)) => {
                            old_data.is_deleted() &&
                            old_data.get_version() + 1 == new_data.get_version()
                        }
                        _ => false,
                    };
                    if !valid {
                        trace!("DM sending PutFailure for data {:?}, it already exists.",
                               data_id);
                    }
                }
                DataIdentifier::Immutable(..) => {
                    trace!("DM sending PutSuccess for data {:?}, it already exists.",
                           data_id);
                    routing_node.send_put_success(dst, src, data_id, message_id)?;
                    return Ok(());
                }
            }
        }

        self.clean_chunk_store();

        let is_full = self.chunk_store_full();

        let error_opt = if !valid {
            Some(MutationError::DataExists)
        } else if is_full {
            Some(MutationError::NetworkFull)
        } else {
            None
        };

        if let Some(error) = error_opt {
            let external_error_indicator = serialisation::serialise(&error)?;
            routing_node
                .send_put_failure(dst, src, data_id, external_error_indicator, message_id)?;
            if is_full {
                return Err(From::from(error));
            }
        }

        self.update_pending_writes(routing_node, data, PendingMutationType::Put, src, dst, message_id, !valid)
    }

    /// Handles a Post request for structured or appendable data.
    pub fn handle_post(&mut self,
                       routing_node: &mut RoutingNode,
                       src: Authority<XorName>,
                       dst: Authority<XorName>,
                       new_data: Data,
                       message_id: MessageId)
                       -> Result<(), InternalError> {
        let data_id = new_data.identifier();

        if !new_data.validate_size() {
            let error = MutationError::DataTooLarge;
            let post_error = serialisation::serialise(&error)?;
            trace!("DM sending post_failure for data {:?}, data exceeds size limit.",
                   data_id);
            return Ok(routing_node
                .send_post_failure(dst, src, data_id, post_error, message_id)?);
        }

        let mut error_opt = None;
        let update_result = match (new_data, self.chunk_store.get(&data_id)) {
            (Data::Structured(new_sd), Err(_)) => {
                warn!("Post operation for nonexistent data. {:?} - {:?}",
                      data_id,
                      message_id);
                error_opt = Some(MutationError::NoSuchData);
                Ok(Data::Structured(new_sd))
            }
            (_, Err(_)) => Err(MutationError::NoSuchData),
            (Data::Structured(new_sd), Ok(Data::Structured(mut sd))) => {
                if sd.is_deleted() {
                    warn!("Post operation for deleted data. {:?} - {:?}",
                          data_id,
                          message_id);
                    error_opt = Some(MutationError::InvalidOperation);
                } else if sd.replace_with_other(new_sd.clone()).is_err() {
                    error_opt = Some(MutationError::InvalidSuccessor);
                }
                Ok(Data::Structured(new_sd))
            }
            (Data::PubAppendable(new_ad), Ok(Data::PubAppendable(mut ad))) =>
                ad.update_with_other(new_ad)
                  .map(|()| Data::PubAppendable(ad))
                  .map_err(|_| MutationError::InvalidSuccessor),
            (Data::PrivAppendable(new_ad), Ok(Data::PrivAppendable(mut ad))) =>
                ad.update_with_other(new_ad)
                  .map(|()| Data::PrivAppendable(ad))
                  .map_err(|_| MutationError::InvalidSuccessor),
            (_, Ok(_)) => {
                warn!("Post operation for Invalid Data Type. {:?} - {:?}",
                      data_id,
                      message_id);
                Err(MutationError::InvalidOperation)
            }
        };

        let data = match update_result {
            Ok(data) => data,
            Err(error) => {
                trace!("DM sending post_failure for: {:?} with {:?} - {:?}",
                       data_id,
                       message_id,
                       error);
                let post_error = serialisation::serialise(&error)?;
                return Ok(routing_node
                    .send_post_failure(dst, src, data_id, post_error, message_id)?);
            }
        };

        if let Some(ref error) = error_opt {
            let post_error = serialisation::serialise(error)?;
            routing_node.send_post_failure(dst, src, data_id, post_error, message_id)?;
        }
        self.update_pending_writes(routing_node,
                                   data,
                                   PendingMutationType::Post,
                                   src,
                                   dst,
                                   message_id,
                                   error_opt.is_some())
    }

    /// The structured_data in the delete request must be a valid updating version of the target
    pub fn handle_delete(&mut self,
                         routing_node: &mut RoutingNode,
                         src: Authority<XorName>,
                         dst: Authority<XorName>,
                         new_data: StructuredData,
                         message_id: MessageId)
                         -> Result<(), InternalError> {
        let data_id = new_data.identifier();

        let error_opt = match self.chunk_store.get(&data_id) {
            Ok(Data::Structured(mut data)) => {
                let error_opt = if data.is_deleted() {
                    Some(MutationError::InvalidOperation)
                } else if data.delete_if_valid_successor(&new_data).is_ok() {
                    None
                } else {
                    Some(MutationError::InvalidSuccessor)
                };
                self.update_pending_writes(routing_node,
                    Data::Structured(data),
                                           PendingMutationType::Delete,
                                           src,
                                           dst,
                                           message_id,
                                           error_opt.is_some())?;
                error_opt
            }
            Ok(_) => Some(MutationError::InvalidOperation),
            Err(_) => Some(MutationError::NoSuchData),
        };
        if let Some(error) = error_opt {
            trace!("DM sending delete_failure for {:?}", new_data.identifier());
            let err_data = serialisation::serialise(&error)?;
            routing_node.send_delete_failure(dst, src, data_id, err_data, message_id)?;
        }
        Ok(())
    }

    /// Handles a request to append an item to a public or private appendable data chunk.
    pub fn handle_append(&mut self,
                         routing_node: &mut RoutingNode,
                         src: Authority<XorName>,
                         dst: Authority<XorName>,
                         wrapper: AppendWrapper,
                         message_id: MessageId)
                         -> Result<(), InternalError> {
        let data_id = wrapper.identifier();
        let append_result = match (wrapper, self.chunk_store.get(&data_id)) {
            (wrapper @ AppendWrapper::Pub { .. }, Ok(Data::PubAppendable(mut ad))) => {
                if ad.apply_wrapper(wrapper) {
                    Some(Data::PubAppendable(ad))
                } else {
                    None
                }
            }
            (wrapper @ AppendWrapper::Priv { .. }, Ok(Data::PrivAppendable(mut ad))) => {
                if ad.apply_wrapper(wrapper) {
                    Some(Data::PrivAppendable(ad))
                } else {
                    None
                }
            }
            (_, Ok(_)) => {
                unreachable!("Append operation for Invalid Data Type. {:?} - {:?}",
                             data_id,
                             message_id)
            }
            (_, Err(error)) => {
                trace!("DM sending append_failure for: {:?} with {:?} - {:?}",
                       data_id,
                       message_id,
                       error);
                let append_error = serialisation::serialise(&MutationError::NoSuchData)?;
                return Ok(routing_node
                    .send_append_failure(dst, src, data_id, append_error, message_id)?);
            }
        };

        if let Some(data) = append_result {
            if !data.validate_size() {
                let error = MutationError::DataTooLarge;
                let append_error = serialisation::serialise(&error)?;
                trace!("DM sending append_failure for data {:?}, data exceeds size limit.",
                       data_id);
                return Ok(routing_node
                    .send_append_failure(dst, src, data_id, append_error, message_id)?);
            }
            self.update_pending_writes(routing_node,data,

                                       PendingMutationType::Append,
                                       src,
                                       dst,
                                       message_id,
                                       false)
        } else {
            trace!("DM sending append_failure for: {:?} with {:?}",
                   data_id,
                   message_id);
            let append_error = serialisation::serialise(&MutationError::InvalidSuccessor)?;
            Ok(routing_node
                .send_append_failure(dst, src, data_id, append_error, message_id)?)
        }
    }

    pub fn handle_get_success(&mut self,
                              routing_node: &mut RoutingNode,
                              src: XorName,
                              mut data: Data)
                              -> Result<(), InternalError> {
        let (data_id, version) = id_and_version_of(&data);
        self.cache.handle_get_success(src, &data_id, version);
        self.send_gets_for_needed_data(routing_node)?;
        // If we're no longer in the close group, return.
        if !self.close_to_address(routing_node, data_id.name()) {
            return Ok(());
        }
        // TODO: Check that the data's hash actually agrees with an accumulated entry.
        let mut got_new_data = true;
        match data_id {
            DataIdentifier::PubAppendable(..) => {
                if let Ok(Data::PubAppendable(appendable_data)) = self.chunk_store.get(&data_id) {
                    // Make sure we don't 'update' to a lower version.
                    if appendable_data.get_version() > version {
                        return Ok(());
                    }
                    if appendable_data.get_version() == version {
                        if let Data::PubAppendable(ref mut received) = data {
                            received.data.extend(appendable_data.data.into_iter());
                        } else {
                            unreachable!("DataIdentifier variant and Data variant mismatch");
                        }
                    }
                    got_new_data = false;
                }
            }
            DataIdentifier::PrivAppendable(..) => {
                if let Ok(Data::PrivAppendable(appendable_data)) = self.chunk_store.get(&data_id) {
                    // Make sure we don't 'update' to a lower version.
                    if appendable_data.get_version() > version {
                        return Ok(());
                    }
                    if appendable_data.get_version() == version {
                        if let Data::PrivAppendable(ref mut received) = data {
                            received.data.extend(appendable_data.data.into_iter());
                        } else {
                            unreachable!("DataIdentifier variant and Data variant mismatch");
                        }
                    }
                    got_new_data = false;
                }
            }
            DataIdentifier::Structured(..) => {
                if let Ok(Data::Structured(structured_data)) = self.chunk_store.get(&data_id) {
                    // Make sure we don't 'update' to a lower version.
                    if structured_data.get_version() >= version {
                        return Ok(());
                    }
                    got_new_data = false;
                }
            }
            DataIdentifier::Immutable(..) => {
                if self.chunk_store.has(&data_id) {
                    return Ok(()); // Immutable data is already there.
                }
            }
        }

        self.clean_chunk_store();
        // chunk_store::put() deletes the old data automatically.
        self.chunk_store.put(&data_id, &data)?;
        if got_new_data {
            self.count_added_data(&data_id);
            if self.logging_time.elapsed().as_secs() > STATUS_LOG_INTERVAL {
                self.logging_time = Instant::now();
                info!("{:?}", self);
            }
        }
        Ok(())
    }

    pub fn handle_get_failure(&mut self,
                              routing_node: &mut RoutingNode,
                              src: XorName,
                              data_id: DataIdentifier)
                              -> Result<(), InternalError> {
        if !self.cache.handle_get_failure(src, &data_id) {
            warn!("Got unexpected GetFailure for data {:?}.", data_id);
            return Err(InternalError::InvalidMessage);
        }
        self.send_gets_for_needed_data(routing_node)
    }

    pub fn handle_refresh(&mut self,
                          routing_node: &mut RoutingNode,
                          src: XorName,
                          serialised_data_list: &[u8])
                          -> Result<(), InternalError> {
        let RefreshDataList(data_list) = serialisation::deserialise(serialised_data_list)?;
        for data_idv in data_list {
            if self.cache.register_data_with_holder(&src, &data_idv) {
                continue;
            }
            if let Some(holders) = self.refresh_accumulator.add(data_idv, src).cloned() {
                self.refresh_accumulator.delete(&data_idv);
                let (ref data_id, ref version) = data_idv;
                let data_needed = match *data_id {
                    DataIdentifier::Immutable(..) => !self.chunk_store.has(data_id),
                    DataIdentifier::Structured(..) => {
                        match self.chunk_store.get(data_id) {
                            // We don't have the data, so we need to retrieve it.
                            Err(_) => true,
                            Ok(Data::Structured(sd)) => sd.get_version() < *version,
                            Ok(_) => unreachable!(),
                        }
                    }
                    DataIdentifier::PubAppendable(..) => {
                        match self.chunk_store.get(data_id) {
                            // We don't have the data, so we need to retrieve it.
                            Err(_) => true,
                            Ok(Data::PubAppendable(ad)) => ad.get_version() <= *version,
                            Ok(_) => unreachable!(),
                        }
                    }
                    DataIdentifier::PrivAppendable(..) => {
                        match self.chunk_store.get(data_id) {
                            // We don't have the data, so we need to retrieve it.
                            Err(_) => true,
                            Ok(Data::PrivAppendable(ad)) => ad.get_version() <= *version,
                            Ok(_) => unreachable!(),
                        }
                    }
                };
                if !data_needed {
                    continue;
                }
                self.cache.add_records(data_idv, holders);
            }
        }
        self.send_gets_for_needed_data(routing_node)
    }

    /// Handles an accumulated refresh message sent from the whole group.
    pub fn handle_group_refresh(&mut self, routing_node: &mut RoutingNode, serialised_refresh: &[u8]) -> Result<(), InternalError> {
        let RefreshData((data_id, version), refresh_hash) =
            serialisation::deserialise(serialised_refresh)?;
        let mut success = false;
        for PendingWrite { data, mutate_type, src, dst, message_id, hash, rejected, .. } in
            self.cache.take_pending_writes(&data_id) {
            if hash == refresh_hash {
                let already_existed = self.chunk_store.has(&data_id);
                if let Err(error) = self.chunk_store.put(&data_id, &data) {
                    trace!("DM failed to store {:?} in chunkstore: {:?}",
                           data_id,
                           error);
                    let error = MutationError::NetworkOther(format!("Failed to store chunk: {:?}",
                                                                    error));
                    self.send_failure(routing_node, mutate_type, src, dst, data_id, message_id, error)?;
                } else {
                    trace!("DM updated for: {:?}", data_id);
                    let _ = match mutate_type {
                        PendingMutationType::Append => {
                            trace!("DM sending AppendSuccess for data {:?}", data_id);
                            routing_node.send_append_success(dst, src, data_id, message_id)
                        }
                        PendingMutationType::Post => {
                            trace!("DM sending PostSuccess for data {:?}", data_id);
                            routing_node.send_post_success(dst, src, data_id, message_id)
                        }
                        PendingMutationType::Put => {
                            // Put to a deleted data shall not be counted
                            if !already_existed {
                                self.count_added_data(&data_id);
                            }
                            if self.logging_time.elapsed().as_secs() > STATUS_LOG_INTERVAL {
                                self.logging_time = Instant::now();
                                info!("{:?}", self);
                            }
                            trace!("DM sending PutSuccess for data {:?}", data_id);
                            routing_node.send_put_success(dst, src, data_id, message_id)
                        }
                        PendingMutationType::Delete => {
                            trace!("DM sending DeleteSuccess for data {:?}", data_id);
                            routing_node.send_delete_success(dst, src, data_id, message_id)
                        }
                    };
                    let data_list = vec![(data_id, version)];
                    let _ = self.send_refresh(routing_node, Authority::NaeManager(*data_id.name()), data_list);
                    success = true;
                }
            } else if !rejected {
                trace!("{:?} did not accumulate. Sending failure", data_id);
                let error = MutationError::NetworkOther("Concurrent modification.".to_owned());
                self.send_failure(routing_node, mutate_type, src, dst, data.identifier(), message_id, error)?;
            }
        }
        if !success {
            if let Some(group) = routing_node.close_group(*data_id.name(), GROUP_SIZE) {
                let data_idv = (data_id, version);
                for node in &group {
                    let _ = self.cache.register_data_with_holder(node, &data_idv);
                }
                self.send_gets_for_needed_data(routing_node)?;
            }
        }
        Ok(())
    }

    fn send_failure(&self,
                    routing_node: &mut RoutingNode,
                    mutate_type: PendingMutationType,
                    src: Authority<XorName>,
                    dst: Authority<XorName>,
                    data_id: DataIdentifier,
                    message_id: MessageId,
                    error: MutationError)
                    -> Result<(), InternalError> {
        let write_error = serialisation::serialise(&error)?;
        Ok(match mutate_type {
            PendingMutationType::Append => {
                routing_node.send_append_failure(dst, src, data_id, write_error, message_id)
            }
            PendingMutationType::Post => {
                routing_node.send_post_failure(dst, src, data_id, write_error, message_id)
            }
            PendingMutationType::Put => {
                routing_node.send_put_failure(dst, src, data_id, write_error, message_id)
            }
            PendingMutationType::Delete => {
                routing_node.send_delete_failure(dst, src, data_id, write_error, message_id)
            }
        }?)
    }

    fn update_pending_writes(&mut self,
                             routing_node: &mut RoutingNode,
                             data: Data,
                             mutate_type: PendingMutationType,
                             src: Authority<XorName>,
                             dst: Authority<XorName>,
                             message_id: MessageId,
                             rejected: bool)
                             -> Result<(), InternalError> {
        for PendingWrite { mutate_type, src, dst, data, message_id, .. } in
            self.cache.remove_expired_writes() {
            let data_id = data.identifier();
            let error = MutationError::NetworkOther("Request expired.".to_owned());
            trace!("{:?} did not accumulate. Sending failure", data_id);
            self.send_failure(routing_node, mutate_type, src, dst, data_id, message_id, error)?;
        }
        let data_name = *data.name();

        if let Some(refresh_data) =
            self.cache.insert_pending_write(data, mutate_type, src, dst, message_id, rejected) {
            let _ = self.send_group_refresh(routing_node, data_name, refresh_data, message_id);
        }
        Ok(())
    }

    fn send_gets_for_needed_data(&mut self, routing_node: &mut RoutingNode) -> Result<(), InternalError> {
        let src = Authority::ManagedNode(routing_node.name()?.clone());
        let candidates = self.cache.needed_data();
        for (idle_holder, data_idv) in candidates {
            if let Some(group) = routing_node.close_group(*data_idv.0.name(), GROUP_SIZE) {
                if group.contains(&idle_holder) {
                    self.cache.insert_into_ongoing_gets(&idle_holder, &data_idv);
                    let (data_id, _) = data_idv;
                    let dst = Authority::ManagedNode(idle_holder);
                    let msg_id = MessageId::new();
                    let _ = routing_node.send_get_request(src, dst, data_id, msg_id);
                }
            }
        }
        self.cache.print_stats();
        Ok(())
    }

    fn close_to_address(&self, routing_node: &mut RoutingNode, address: &XorName) -> bool {
        routing_node.close_group(*address, GROUP_SIZE).is_some()
    }

    pub fn handle_node_added(&mut self,
                             routing_node: &mut RoutingNode,
                             node_name: &XorName,
                             routing_table: &RoutingTable<XorName>) {
        self.cache.prune_data_holders(routing_table);
        if self.cache.prune_ongoing_gets(routing_table) {
            let _ = self.send_gets_for_needed_data(routing_node);
        }
        let data_idvs = self.cache.chain_records_in_cache(self.chunk_store
            .keys()
            .into_iter()
            .filter_map(|data_id| self.to_id_and_version(data_id)));
        let mut has_pruned_data = false;
        // Only retain data for which we're still in the close group.
        let mut data_list = Vec::new();
        for (data_id, version) in data_idvs {
            match routing_table.other_closest_names(data_id.name(), GROUP_SIZE) {
                None => {
                    trace!("No longer a DM for {:?}", data_id);
                    if self.chunk_store.has(&data_id) && !self.cache.is_in_unneeded(&data_id) {
                        self.count_removed_data(&data_id);
                        has_pruned_data = true;
                        if let DataIdentifier::Immutable(..) = data_id {
                            self.cache.add_as_unneeded(data_id);
                        } else {
                            let _ = self.chunk_store.delete(&data_id);
                        }
                    }
                }
                Some(close_group) => {
                    if close_group.contains(&node_name) {
                        data_list.push((data_id, version));
                    }
                }
            }
        }
        if !data_list.is_empty() {
            let _ = self.send_refresh(routing_node, Authority::ManagedNode(*node_name), data_list);
        }
        if has_pruned_data && self.logging_time.elapsed().as_secs() > STATUS_LOG_INTERVAL {
            self.logging_time = Instant::now();
            info!("{:?}", self);
        }
    }

    /// Get all names and hashes of all data. // [TODO]: Can be optimised - 2016-04-23 09:11pm
    /// Send to all members of group of data.
    pub fn handle_node_lost(&mut self,
                            routing_node: &mut RoutingNode,
                            node_name: &XorName,
                            routing_table: &RoutingTable<XorName>) {
        let pruned_unneeded_chunks = self.cache.prune_unneeded_chunks(routing_table);
        if pruned_unneeded_chunks != 0 {
            self.immutable_data_count += pruned_unneeded_chunks;
            if self.logging_time.elapsed().as_secs() > STATUS_LOG_INTERVAL {
                self.logging_time = Instant::now();
                info!("{:?}", self);
            }
        }
        self.cache.prune_data_holders(routing_table);
        if self.cache.prune_ongoing_gets(routing_table) {
            let _ = self.send_gets_for_needed_data(routing_node);
        }


        let data_idvs = self.cache.chain_records_in_cache(self.chunk_store
            .keys()
            .into_iter()
            .filter_map(|data_id| self.to_id_and_version(data_id))
            .collect_vec());
        let mut data_lists: HashMap<XorName, Vec<IdAndVersion>> = HashMap::new();
        for data_idv in data_idvs {
            match routing_table.other_closest_names(data_idv.0.name(), GROUP_SIZE) {
                None => {
                    error!("Moved out of close group of {:?} in a NodeLost event!",
                           node_name);
                }
                Some(close_group) => {
                    // If no new node joined the group due to this event, continue:
                    // If the group has fewer than GROUP_SIZE elements, the lost node was not
                    // replaced at all. Otherwise, if the group's last node is closer to the data
                    // than the lost node, the lost node was not in the group in the first place.
                    if let Some(&outer_node) = close_group.get(GROUP_SIZE - 2) {
                        if data_idv.0.name().closer(node_name, outer_node) {
                            data_lists.entry(*outer_node).or_insert_with(Vec::new).push(data_idv);
                        }
                    }
                }
            }
        }
        for (node_name, data_list) in data_lists {
            let _ = self.send_refresh(routing_node, Authority::ManagedNode(node_name), data_list);
        }
    }

    pub fn check_timeouts(&mut self, routing_node: &mut RoutingNode) {
        let _ = self.send_gets_for_needed_data(routing_node);
    }

    #[cfg(feature = "use-mock-crust")]
    pub fn get_stored_names(&self) -> Vec<IdAndVersion> {
        let (front, back) = self.cache.unneeded_chunks.as_slices();
        self.chunk_store
            .keys()
            .into_iter()
            .filter(|data_id| !front.contains(data_id) && !back.contains(data_id))
            .filter_map(|data_id| self.to_id_and_version(data_id))
            .collect()
    }

    /// Returns the `IdAndVersion` for the given data identifier, or `None` if not stored.
    fn to_id_and_version(&self, data_id: DataIdentifier) -> Option<IdAndVersion> {
        match data_id {
            DataIdentifier::Immutable(_) => Some((data_id, 0)),
            DataIdentifier::Structured(..) |
            DataIdentifier::PrivAppendable(..) |
            DataIdentifier::PubAppendable(..) => {
                match self.chunk_store.get(&data_id) {
                    Ok(Data::Structured(data)) => Some((data_id, data.get_version())),
                    Ok(Data::PubAppendable(data)) => Some((data_id, data.get_version())),
                    Ok(Data::PrivAppendable(data)) => Some((data_id, data.get_version())),
                    _ => {
                        error!("Failed to get {:?} from chunk store.", data_id);
                        None
                    }
                }
            }
        }
    }

    fn count_added_data(&mut self, data_id: &DataIdentifier) {
        match *data_id {
            DataIdentifier::Immutable(_) => self.immutable_data_count += 1,
            DataIdentifier::Structured(..) => self.structured_data_count += 1,
            DataIdentifier::PubAppendable(..) |
            DataIdentifier::PrivAppendable(..) => self.appendable_data_count += 1,
        }
    }

    fn count_removed_data(&mut self, data_id: &DataIdentifier) {
        match *data_id {
            DataIdentifier::Immutable(_) => self.immutable_data_count -= 1,
            DataIdentifier::Structured(_, _) => self.structured_data_count -= 1,
            DataIdentifier::PubAppendable(..) |
            DataIdentifier::PrivAppendable(..) => self.appendable_data_count -= 1,
        }
    }

    /// Returns whether our data uses more than `MAX_FULL_PERCENT` percent of available space.
    fn chunk_store_full(&self) -> bool {
        self.chunk_store.used_space() > (self.chunk_store.max_space() / 100) * MAX_FULL_PERCENT
    }

    /// Removes data chunks we are no longer responsible for until the chunk store is not full
    /// anymore.
    fn clean_chunk_store(&mut self) {
        while self.chunk_store_full() {
            if let Some(data_id) = self.cache.pop_unneeded_chunk() {
                let _ = self.chunk_store.delete(&data_id);
            } else {
                break;
            }
        }
    }

    fn send_refresh(&self,
                    routing_node: &mut RoutingNode,
                    dst: Authority<XorName>,
                    data_list: Vec<IdAndVersion>)
                    -> Result<(), InternalError> {
        let src = Authority::ManagedNode(routing_node.name()?.clone());
        // FIXME - We need to handle >2MB chunks
        match serialisation::serialise(&RefreshDataList(data_list)) {
            Ok(serialised_list) => {
                trace!("DM sending refresh to {:?}.", dst);
                let _ = routing_node
                    .send_refresh_request(src, dst, serialised_list, MessageId::new());
                Ok(())
            }
            Err(error) => {
                warn!("Failed to serialise data list: {:?}", error);
                Err(From::from(error))
            }
        }
    }

    fn send_group_refresh(&self,
                          routing_node: &mut RoutingNode,
                          name: XorName,
                          refresh_data: RefreshData,
                          msg_id: MessageId)
                          -> Result<(), InternalError> {
        match serialisation::serialise(&refresh_data) {
            Ok(serialised_data) => {
                trace!("DM sending refresh data to group {:?}.", name);
                let _ = routing_node
                    .send_refresh_request(Authority::NaeManager(name),
                                          Authority::NaeManager(name),
                                          serialised_data,
                                          msg_id);
                Ok(())
            }
            Err(error) => {
                warn!("Failed to serialise data: {:?}", error);
                Err(From::from(error))
            }
        }
    }
}

/// A list of data held by the sender. Sent from node to node.
#[derive(RustcEncodable, RustcDecodable, PartialEq, Eq, Debug, Clone)]
struct RefreshDataList(Vec<IdAndVersion>);

/// A message from the group to itself to store the given data. If this accumulates, that means a
/// quorum of group members approves.
#[derive(RustcEncodable, RustcDecodable, PartialEq, Eq, Debug, Copy, Clone)]
struct RefreshData(IdAndVersion, u64);
