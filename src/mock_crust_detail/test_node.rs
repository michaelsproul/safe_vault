// Copyright 2016 MaidSafe.net limited.
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

use super::poll;
use config_handler::Config;
use hex::ToHex;
use personas::data_manager::IdAndVersion;
use rand::{self, Rng};
use routing::{RoutingTable, XorName};
use routing::mock_crust::{self, Endpoint, Network, ServiceHandle};
use std::env;
use std::fs;
use std::path::PathBuf;
use vault::Vault;

/// Test node for mock network
pub struct TestNode {
    handle: ServiceHandle,
    vault: Vault,
    chunk_store_root: PathBuf,
}

impl TestNode {
    /// create a test node for mock network
    pub fn new(network: &Network,
               crust_config: Option<mock_crust::Config>,
               config: Option<Config>,
               first_node: bool,
               use_cache: bool,
               evil: bool)
               -> Self {
        let handle = network.new_service_handle(crust_config, None);
        let temp_root = env::temp_dir();
        let chunk_store_root = temp_root.join(rand::thread_rng()
                                                  .gen_iter()
                                                  .take(8)
                                                  .collect::<Vec<u8>>()
                                                  .to_hex());
        let mut vault_config = config.unwrap_or_default();
        vault_config.chunk_store_root = Some(format!("{}", chunk_store_root.display()));
        let vault = mock_crust::make_current(&handle, || {
            unwrap!(Vault::new_with_config(first_node, use_cache, vault_config, evil))
        });
        TestNode {
            handle: handle,
            vault: vault,
            chunk_store_root: chunk_store_root,
        }
    }
    /// Empty the event queue for this node on the mock network
    pub fn poll(&mut self) -> usize {
        let mut result = 0;

        while self.vault.poll() {
            result += 1;
        }

        result
    }

    /// empty this client event loop
    pub fn poll_once(&mut self) -> bool {
        self.vault.poll()
    }

    /// Return endpoint for this node
    pub fn endpoint(&self) -> Endpoint {
        self.handle.endpoint()
    }

    /// return names of all data stored on mock network
    pub fn get_stored_names(&self) -> Vec<IdAndVersion> {
        self.vault.get_stored_names()
    }

    /// return the number of account packets stored for the given client
    pub fn get_maid_manager_put_count(&self, client_name: &XorName) -> Option<u64> {
        self.vault.get_maid_manager_put_count(client_name)
    }

    /// Resend all unacknowledged messages.
    pub fn resend_unacknowledged(&mut self) -> bool {
        self.vault.resend_unacknowledged()
    }

    /// Clear routing node state..
    pub fn clear_state(&mut self) {
        self.vault.clear_state()
    }

    /// name of vault.
    pub fn name(&self) -> XorName {
        self.vault.name()
    }

    /// returns the vault's routing_table.
    pub fn routing_table(&self) -> RoutingTable<XorName> {
        self.vault.routing_table()
    }
}

/// Create nodes for mock network
pub fn create_nodes(network: &Network,
                    size: usize,
                    config: Option<&Config>,
                    use_cache: bool)
                    -> Vec<TestNode> {
    let mut nodes = Vec::new();

    // Create the seed node.
    nodes.push(TestNode::new(network, None, config.cloned(), true, use_cache, false));
    while nodes[0].poll() > 0 {}

    let crust_config = mock_crust::Config::with_contacts(&[nodes[0].endpoint()]);

    // Create other nodes using the seed node endpoint as bootstrap contact.
    for _ in 1..size {
        nodes.push(TestNode::new(network,
                                 Some(crust_config.clone()),
                                 config.cloned(),
                                 false,
                                 use_cache,
                                 false));
        poll::nodes(&mut nodes);
    }

    nodes
}

/// Add an evil node to the network.
pub fn add_evil_node(network: &Network, nodes: &mut Vec<TestNode>) {
    let crust_config = mock_crust::Config::with_contacts(&[nodes[0].endpoint()]);
    nodes.push(TestNode::new(network, Some(crust_config), None, false, false, true));
}

/// Add node to the mock network
pub fn add_node(network: &Network, nodes: &mut Vec<TestNode>, index: usize, use_cache: bool) {
    let config = mock_crust::Config::with_contacts(&[nodes[index].endpoint()]);
    nodes.push(TestNode::new(network, Some(config.clone()), None, false, use_cache, false));
}

/// Add node to the mock network with specified config
pub fn add_node_with_config(network: &Network,
                            nodes: &mut Vec<TestNode>,
                            config: Config,
                            index: usize,
                            use_cache: bool) {
    let crust_config = mock_crust::Config::with_contacts(&[nodes[index].endpoint()]);
    nodes.push(TestNode::new(network, Some(crust_config), Some(config), false, use_cache, false));
}

/// remove this node from the mock network
pub fn drop_node(nodes: &mut Vec<TestNode>, index: usize) {
    let node = nodes.remove(index);
    trace!("Removing node: {:?}", node.name());
    drop(node);
}

/// Process all events
fn _poll_all(nodes: &mut [TestNode]) {
    while nodes.iter_mut().any(|node| node.poll() > 0) {}
}

impl Drop for TestNode {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.chunk_store_root);
    }
}
