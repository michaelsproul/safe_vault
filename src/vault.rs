// Copyright 2015 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under (1) the MaidSafe.net Commercial License,
// version 1.0 or later, or (2) The General Public License (GPL), version 3, depending on which
// licence you accepted on initial access to the Software (the "Licences").
//
// By contributing code to the SAFE Network Software, or to this project generally, you agree to be
// bound by the terms of the MaidSafe Contributor Agreement.  This, along with the Licenses can be
// found in the root directory of this project at LICENSE, COPYING and CONTRIBUTOR.
//
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.
//
// Please review the Licences for the specific language governing permissions and limitations
// relating to use of the SAFE Network Software.

use GROUP_SIZE;
use cache::Cache;
use config_handler::{self, Config};
use error::InternalError;
use personas::data_manager::DataManager;
#[cfg(feature = "use-mock-crust")]
use personas::data_manager::IdAndVersion;
use personas::maid_manager::MaidManager;
use routing::{Authority, Data, EventStream, NodeBuilder, Request, Response, RoutingTable, XorName};
use rust_sodium;
use rust_sodium::crypto::sign::PublicKey;
use std::env;
use std::path::Path;

pub const CHUNK_STORE_DIR: &'static str = "safe_vault_chunk_store";
const DEFAULT_MAX_CAPACITY: u64 = 2 * 1024 * 1024 * 1024;

pub use routing::Event;
pub use routing::Node as RoutingNode;

/// Main struct to hold all personas and Routing instance
pub struct Vault {
    maid_manager: MaidManager,
    data_manager: DataManager,
    routing_node: RoutingNode,
}

impl Vault {
    /// Creates a network Vault instance.
    pub fn new(first_vault: bool, use_cache: bool) -> Result<Self, InternalError> {
        let config = match config_handler::read_config_file() {
            Ok(cfg) => cfg,
            Err(InternalError::FileHandler(e)) => {
                error!("Config file could not be parsed: {:?}", e);
                return Err(From::from(e));
            }
            Err(e) => return Err(From::from(e)),
        };
        let builder = RoutingNode::builder()
            .first(first_vault)
            .deny_other_local_nodes();
        match Self::vault_with_config(builder, use_cache, config.clone(), false) {
            Ok(vault) => Ok(vault),
            Err(InternalError::ChunkStore(e)) => {
                error!("Incorrect path {:?} for chunk_store_root: {:?}",
                       config.chunk_store_root,
                       e);
                Err(From::from(e))
            }
            Err(e) => Err(From::from(e)),
        }
    }

    /// Allow construct vault with config for mock-crust tests.
    #[cfg(feature = "use-mock-crust")]
    pub fn new_with_config(first_vault: bool,
                           use_cache: bool,
                           config: Config,
                           evil: bool)
                           -> Result<Self, InternalError> {
        let node = RoutingNode::builder()
            .first(first_vault)
            .evil(evil);
        Self::vault_with_config(node, use_cache, config, evil)
    }

    /// Allow construct vault with config for mock-crust tests.
    fn vault_with_config(builder: NodeBuilder,
                         use_cache: bool,
                         config: Config,
                         evil: bool)
                         -> Result<Self, InternalError> {
        rust_sodium::init();

        let mut chunk_store_root = match config.chunk_store_root {
            Some(path_str) => Path::new(&path_str).to_path_buf(),
            None => env::temp_dir(),
        };
        chunk_store_root.push(CHUNK_STORE_DIR);

        let routing_node = if use_cache {
            builder.cache(Box::new(Cache::new())).create(GROUP_SIZE)
        } else {
            builder.create(GROUP_SIZE)
        }?;

        Ok(Vault {
               maid_manager: MaidManager::new(config.invite_key.map(PublicKey)),
               data_manager: DataManager::new(chunk_store_root,
                                              config.max_capacity.unwrap_or(DEFAULT_MAX_CAPACITY),
                                              evil)?,
               routing_node: routing_node,
           })

    }

    /// Run the event loop, processing events received from Routing.
    pub fn run(&mut self) -> Result<bool, InternalError> {
        while let Ok(ev) = self.routing_node.next_ev() {
            if let Some(terminate) = self.process_event(ev) {
                return Ok(terminate);
            }
        }
        // FIXME: decide if we want to restart here (in which case return `Ok(false)`).
        Ok(true)
    }

    /// Non-blocking call to process any events in the event queue, returning true if
    /// any received, otherwise returns false.
    #[cfg(feature = "use-mock-crust")]
    pub fn poll(&mut self) -> bool {
        let mut ev_processed = self.routing_node.poll();

        while let Ok(ev) = self.routing_node.try_next_ev() {
            let _ = self.process_event(ev);
            ev_processed = true;
        }

        ev_processed
    }

    /// Get the names of all the data chunks stored in a personas' chunk store.
    #[cfg(feature = "use-mock-crust")]
    pub fn get_stored_names(&self) -> Vec<IdAndVersion> {
        self.data_manager.get_stored_names()
    }

    /// Get the number of put requests the network processed for the given client.
    #[cfg(feature = "use-mock-crust")]
    pub fn get_maid_manager_put_count(&self, client_name: &XorName) -> Option<u64> {
        self.maid_manager.get_put_count(client_name)
    }

    /// Resend all unacknowledged messages.
    #[cfg(feature = "use-mock-crust")]
    pub fn resend_unacknowledged(&mut self) -> bool {
        self.routing_node.resend_unacknowledged()
    }

    /// Clear routing node state.
    #[cfg(feature = "use-mock-crust")]
    pub fn clear_state(&mut self) {
        self.routing_node.clear_state()
    }

    /// Vault node name
    #[cfg(feature = "use-mock-crust")]
    pub fn name(&self) -> XorName {
        unwrap!(self.routing_node.name())
    }

    /// Vault routing_table
    #[cfg(feature = "use-mock-crust")]
    pub fn routing_table(&self) -> RoutingTable<XorName> {
        unwrap!(self.routing_node.routing_table()).clone()
    }

    fn process_event(&mut self, event: Event) -> Option<bool> {
        let mut ret = None;

        if let Err(error) = match event {
               Event::Request { request, src, dst } => self.on_request(request, src, dst),
               Event::Response { response, src, dst } => self.on_response(response, src, dst),
               Event::NodeAdded(node_added, routing_table) => {
                   self.on_node_added(node_added, routing_table)
               }
               Event::NodeLost(node_lost, routing_table) => {
                   self.on_node_lost(node_lost, routing_table)
               }
               Event::RestartRequired => {
            warn!("Restarting Vault");
            ret = Some(false);
            Ok(())
        }
               Event::Terminate => {
            ret = Some(true);
            Ok(())
        }
               Event::SectionSplit(_prefix) |
               Event::SectionMerge(_prefix) => Ok(()),
               Event::Connected | Event::Tick => Ok(()),
           } {
            debug!("Failed to handle event: {:?}", error);
        }

        self.data_manager.check_timeouts(&mut self.routing_node);
        ret
    }

    fn on_request(&mut self,
                  request: Request,
                  src: Authority<XorName>,
                  dst: Authority<XorName>)
                  -> Result<(), InternalError> {
        match (src, dst, request) {
            // ================== Get ==================
            (src @ Authority::Client { .. },
             dst @ Authority::NaeManager(_),
             Request::Get(data_id, msg_id)) |
            (src @ Authority::ManagedNode(_),
             dst @ Authority::ManagedNode(_),
             Request::Get(data_id, msg_id)) => {
                self.data_manager
                    .handle_get(&mut self.routing_node, src, dst, data_id, msg_id)
            }
            // ================== Put ==================
            (src @ Authority::Client { .. },
             dst @ Authority::ClientManager(_),
             Request::Put(data, msg_id)) => {
                self.maid_manager
                    .handle_put(&mut self.routing_node, src, dst, data, msg_id)
            }
            (src @ Authority::ClientManager(_),
             dst @ Authority::NaeManager(_),
             Request::Put(data, msg_id)) => {
                self.data_manager
                    .handle_put(&mut self.routing_node, src, dst, data, msg_id)
            }
            // ================== Post ==================
            (src @ Authority::ClientManager(_),
             dst @ Authority::NaeManager(_),
             Request::Post(data, msg_id)) |
            (src @ Authority::Client { .. },
             dst @ Authority::NaeManager(_),
             Request::Post(data, msg_id)) => {
                self.data_manager
                    .handle_post(&mut self.routing_node, src, dst, data, msg_id)
            }
            // ================== Delete ==================
            (src @ Authority::Client { .. },
             dst @ Authority::NaeManager(_),
             Request::Delete(Data::Structured(data), msg_id)) => {
                self.data_manager
                    .handle_delete(&mut self.routing_node, src, dst, data, msg_id)
            }
            // ================== Append ==================
            (src @ Authority::Client { .. },
             dst @ Authority::NaeManager(_),
             Request::Append(wrapper, msg_id)) => {
                self.data_manager
                    .handle_append(&mut self.routing_node, src, dst, wrapper, msg_id)
            }
            // ================== GetAccountInfo ==================
            (src @ Authority::Client { .. },
             dst @ Authority::ClientManager(_),
             Request::GetAccountInfo(msg_id)) => {
                self.maid_manager
                    .handle_get_account_info(&mut self.routing_node, src, dst, msg_id)
            }
            // ================== Refresh ==================
            (Authority::ClientManager(_),
             Authority::ClientManager(_),
             Request::Refresh(serialised_msg, _)) => {
                self.maid_manager
                    .handle_refresh(&mut self.routing_node, &serialised_msg)
            }
            (Authority::ManagedNode(src_name),
             Authority::ManagedNode(_),
             Request::Refresh(serialised_msg, _)) |
            (Authority::ManagedNode(src_name),
             Authority::NaeManager(_),
             Request::Refresh(serialised_msg, _)) => {
                self.data_manager
                    .handle_refresh(&mut self.routing_node, src_name, &serialised_msg)
            }
            (Authority::NaeManager(_),
             Authority::NaeManager(_),
             Request::Refresh(serialised_msg, _)) => {
                self.data_manager
                    .handle_group_refresh(&mut self.routing_node, &serialised_msg)
            }
            // ================== Invalid Request ==================
            (_, _, request) => Err(InternalError::UnknownRequestType(request)),
        }
    }

    fn on_response(&mut self,
                   response: Response,
                   src: Authority<XorName>,
                   dst: Authority<XorName>)
                   -> Result<(), InternalError> {
        match (src, dst, response) {
            // ================== GetSuccess ==================
            (Authority::ManagedNode(src_name),
             Authority::ManagedNode(_),
             Response::GetSuccess(data, _)) => {
                self.data_manager
                    .handle_get_success(&mut self.routing_node, src_name, data)
            }
            // ================== GetFailure ==================
            (Authority::ManagedNode(src_name),
             Authority::ManagedNode(_),
             Response::GetFailure { data_id, .. }) => {
                self.data_manager
                    .handle_get_failure(&mut self.routing_node, src_name, data_id)
            }
            // ================== PutSuccess ==================
            (Authority::NaeManager(_),
             Authority::ClientManager(_),
             Response::PutSuccess(data_id, msg_id)) => {
                self.maid_manager
                    .handle_put_success(&mut self.routing_node, data_id, msg_id)
            }
            // ================== PutFailure ==================
            (Authority::NaeManager(_),
             Authority::ClientManager(_),
             Response::PutFailure {
                 id,
                 external_error_indicator,
                 data_id,
             }) => {
                self.maid_manager
                    .handle_put_failure(&mut self.routing_node,
                                        id,
                                        data_id,
                                        &external_error_indicator)
            }
            // ================== PostSuccess ==================
            (Authority::NaeManager(_),
             Authority::ClientManager(client_name),
             Response::PostSuccess(_, msg_id)) => {
                self.maid_manager
                    .handle_post_success(&mut self.routing_node, msg_id, client_name)
            }
            // ================== PostFailure ==================
            (Authority::NaeManager(_),
             Authority::ClientManager(_),
             Response::PostFailure {
                 id,
                 external_error_indicator,
                 ..
             }) => {
                self.maid_manager
                    .handle_post_failure(&mut self.routing_node, id, &external_error_indicator)
            }
            // ================== Invalid Response ==================
            (_, _, response) => Err(InternalError::UnknownResponseType(response)),
        }
    }

    fn on_node_added(&mut self,
                     node_added: XorName,
                     routing_table: RoutingTable<XorName>)
                     -> Result<(), InternalError> {
        self.maid_manager
            .handle_node_added(&mut self.routing_node, &node_added, &routing_table);
        self.data_manager
            .handle_node_added(&mut self.routing_node, &node_added, &routing_table);
        Ok(())
    }

    fn on_node_lost(&mut self,
                    node_lost: XorName,
                    routing_table: RoutingTable<XorName>)
                    -> Result<(), InternalError> {
        self.maid_manager
            .handle_node_lost(&mut self.routing_node, &node_lost);
        self.data_manager
            .handle_node_lost(&mut self.routing_node, &node_lost, &routing_table);
        Ok(())
    }
}
