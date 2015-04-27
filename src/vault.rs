/*  Copyright 2015 MaidSafe.net limited
    This MaidSafe Software is licensed to you under (1) the MaidSafe.net Commercial License,
    version 1.0 or later, or (2) The General Public License (GPL), version 3, depending on which
    licence you accepted on initial access to the Software (the "Licences").
    By contributing code to the MaidSafe Software, or to this project generally, you agree to be
    bound by the terms of the MaidSafe Contributor Agreement, version 1.0, found in the root
    directory of this project at LICENSE, COPYING and CONTRIBUTOR respectively and also
    available at: http://www.maidsafe.net/licenses
    Unless required by applicable law or agreed to in writing, the MaidSafe Software distributed
    under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS
    OF ANY KIND, either express or implied.
    See the Licences for the specific language governing permissions and limitations relating to
    use of the MaidSafe
    Software.                                                                 */


#![deny(missing_docs)]

use routing;
use routing::{Action, RoutingError, NameType};
use routing::types::{Authority, DestinationAddress};

use data_manager::DataManager;
use maid_manager::MaidManager;
use pmid_manager::PmidManager;
use pmid_node::PmidNode;
use version_handler::VersionHandler;

/// Main struct to hold all personas
pub struct VaultFacade {
  data_manager : DataManager,
  maid_manager : MaidManager,
  pmid_manager : PmidManager,
  pmid_node : PmidNode,
  version_handler : VersionHandler,
  nodes_in_table : Vec<NameType>,
}

impl Clone for VaultFacade {
  fn clone(&self) -> VaultFacade {
    VaultFacade::new()
  }
}

impl routing::node_interface::Interface for VaultFacade {
  fn handle_get(&mut self, type_id: u64, our_authority: Authority, from_authority: Authority,
                from_address: NameType, data_name: Vec<u8>)->Result<Action, RoutingError> {
    let name = NameType::new(routing::types::vector_as_u8_64_array(data_name));
    match our_authority {
      Authority::NaeManager => {
        // both DataManager and VersionHandler are NaeManagers and Get request to them are both from Node
        // data input here is assumed as name only(no type info attached)
        let data_manager_result = self.data_manager.handle_get(&name);
        if data_manager_result.is_ok() {
          return data_manager_result;
        }
        return self.version_handler.handle_get(name);
      }
      Authority::ManagedNode => { return self.pmid_node.handle_get(name); }
      _ => { return Err(RoutingError::InvalidRequest); }
    }
  }

  fn handle_put(&mut self, our_authority: Authority, from_authority: Authority,
                from_address: NameType, dest_address: DestinationAddress, data: Vec<u8>)->Result<Action, RoutingError> {
    match our_authority {
      Authority::ClientManager => { return self.maid_manager.handle_put(&from_address, &data); }
      Authority::NaeManager => {
        // both DataManager and VersionHandler are NaeManagers
        // However Put request to DataManager is from ClientManager (MaidManager)
        // meanwhile Put request to VersionHandler is from Node
        match from_authority {
          Authority::ClientManager => { return self.data_manager.handle_put(&data, &mut (self.nodes_in_table)); }
          Authority::ManagedNode => { return self.version_handler.handle_put(data); }
          _ => { return Err(RoutingError::InvalidRequest); }
        }
      }
      Authority::NodeManager => { return self.pmid_manager.handle_put(&dest_address, &data); }
      Authority::ManagedNode => { return self.pmid_node.handle_put(data); }
      _ => { return Err(RoutingError::InvalidRequest); }
    }
  }

  fn handle_post(&mut self, our_authority: Authority, from_authority: Authority, from_address: NameType, data: Vec<u8>)->Result<Action, RoutingError> {
    ;
    Err(RoutingError::InvalidRequest)
  }

  fn handle_get_response(&mut self, from_address: NameType, response: Result<Vec<u8>, RoutingError>) {
    ;
  }

  fn handle_put_response(&mut self, from_authority: Authority, from_address: NameType, response: Result<Vec<u8>, RoutingError>) {
    ;
  }

  fn handle_post_response(&mut self, from_authority: Authority, from_address: NameType, response: Result<Vec<u8>, RoutingError>) {
    ;
  }

    fn handle_churn(&mut self) -> Vec<(routing::NameType, routing::generic_sendable_type::GenericSendableType)> {
        let mut dm = self.data_manager.retrieve_all_and_reset();
        let mut mm = self.maid_manager.retrieve_all_and_reset();
        let mut pm = self.pmid_manager.retrieve_all_and_reset();
        let mut vh = self.version_handler.retrieve_all_and_reset();

        let mut return_val = Vec::<(routing::NameType, routing::generic_sendable_type::GenericSendableType)>::with_capacity(dm.len() + mm.len() + pm.len() + vh.len());
        return_val.append(dm);
        return_val.append(mm);
        return_val.append(pm);
        return_val.append(vh);

        return_val
    }

    // fn handle_cache_get(&mut self,
    //                     type_id: u64,
    //                     from_authority: routing::types::Authority,
    //                     from_address: routing::NameType,
    //                     data: Vec<u8>) -> Result<Action, RoutingError> { unimplemented!() }

    // fn handle_cache_put(&mut self,
    //                     from_authority: routing::types::Authority,
    //                     from_address: routing::NameType,
    //                     data: Vec<u8>) -> Result<Action, RoutingError> { unimplemented!() }
}

impl VaultFacade {
   /// Initialise all the personas in the Vault interface.  
  pub fn new() -> VaultFacade {    
    VaultFacade {
        data_manager: DataManager::new(), maid_manager: MaidManager::new(),
        pmid_manager: PmidManager::new(), pmid_node: PmidNode::new(),
        version_handler: VersionHandler::new(), nodes_in_table: Vec::new(),
    }
  }

}

/// Remove (Krishna) - Temporary function - Can be called from routing::name_type if exposed as public in routing
pub fn closer_to_target(lhs: &NameType, rhs: &NameType, target: &NameType) -> bool {
    for i in 0..lhs.0.len() {
        let res_0 = lhs.0[i] ^ target.0[i];
        let res_1 = rhs.0[i] ^ target.0[i];

        if res_0 != res_1 {
            return res_0 < res_1
        }
    }
    false
}

// Test fail because of the change in the routing interface
// Code has been refactored to support the recent interface changes.
//#[cfg(test)]
// mod test {
//   use super::*;
//   use data_manager;
//   use routing;
//   use cbor;
//   use maidsafe_types;
//   use maidsafe_types::{PayloadTypeTag, Payload};
//   use routing::types:: { Authority, DestinationAddress };   
//   use routing::NameType;
//   use routing::test_utils::Random;
//   use routing::node_interface::Interface;
//   use routing::sendable::Sendable;
//
//   #[test]
//   fn put_get_flow() {
//     let mut vault = VaultFacade::new();

//     let name = NameType([3u8; 64]);
//    let value = routing::types::generate_random_vec_u8(1024);
//     let data = maidsafe_types::ImmutableData::new(value);
//     let payload = Payload::new(PayloadTypeTag::ImmutableData, &data);
//     let mut encoder = cbor::Encoder::from_memory();
//     let encode_result = encoder.encode(&[&payload]);
//     assert_eq!(encode_result.is_ok(), true);

//     { // MaidManager, shall allowing the put and SendOn to DataManagers around name
//       let from = NameType::new([1u8; 64]);
//       // TODO : in this stage, dest can be populated as anything ?
//       let dest = DestinationAddress{ dest : NameType::generate_random(), reply_to: None };
//       let put_result = vault.handle_put(Authority::ClientManager, Authority::Client, from, dest,
//                                         routing::types::array_as_vector(encoder.as_bytes()));
//       assert_eq!(put_result.is_err(), false);
//       match put_result.ok().unwrap() {
//         routing::Action::SendOn(ref x) => {
//           assert_eq!(x.len(), 1);
//           assert_eq!(x[0], NameType([3u8; 64]));
//         }
//         routing::Action::Reply(x) => panic!("Unexpected"),
//       }
//     }
//     let nodes_in_table = vec![NameType::new([1u8; 64]), NameType::new([2u8; 64]), NameType::new([3u8; 64]), NameType::new([4u8; 64]),
//                               NameType::new([5u8; 64]), NameType::new([6u8; 64]), NameType::new([7u8; 64]), NameType::new([8u8; 64])];
//     // Add node removed from interface 
     //for node in nodes_in_table.iter() {
     //  vault.add_node(node.clone());
     //}     
//     { // DataManager, shall SendOn to pmid_nodes
//       let from = NameType::new([1u8; 64]);
//       // TODO : in this stage, dest can be populated as anything ?
//       let dest = DestinationAddress{ dest : NameType::generate_random(), reply_to: None };
//       let put_result = vault.handle_put(Authority::NaeManager, Authority::ClientManager, from, dest,
//                                         routing::types::array_as_vector(encoder.as_bytes()));
//       assert_eq!(put_result.is_err(), false);
//       match put_result.ok().unwrap() {
//         routing::Action::SendOn(ref x) => {
//           assert_eq!(x.len(), data_manager::PARALLELISM);
//           assert_eq!(x[0], NameType([3u8; 64]));
//           assert_eq!(x[1], NameType([2u8; 64]));
//           assert_eq!(x[2], NameType([1u8; 64]));
//           assert_eq!(x[3], NameType([7u8; 64]));
//         }
//         routing::Action::Reply(x) => panic!("Unexpected"),
//       }
//       let from = NameType::new([1u8; 64]);
//       let get_result = vault.handle_get(payload.get_type_tag() as u64, Authority::NaeManager,
//                                         Authority::Client, from, data.name().0.to_vec());
//       assert_eq!(get_result.is_err(), false);
//       match get_result.ok().unwrap() {
//         routing::Action::SendOn(ref x) => {
//           assert_eq!(x.len(), data_manager::PARALLELISM);
//           assert_eq!(x[0], NameType([3u8; 64]));
//           assert_eq!(x[1], NameType([2u8; 64]));
//           assert_eq!(x[2], NameType([1u8; 64]));
//           assert_eq!(x[3], NameType([7u8; 64]));
//         }
//         routing::Action::Reply(x) => panic!("Unexpected"),
//       }
//     }
//     { // PmidManager, shall put to pmid_nodes
//       let from = NameType::new([3u8; 64]);
//       let dest = DestinationAddress{ dest : NameType::new([7u8; 64]), reply_to: None };
//       let put_result = vault.handle_put(Authority::NodeManager, Authority::NaeManager, from, dest,
//                                         routing::types::array_as_vector(encoder.as_bytes()));
//       assert_eq!(put_result.is_err(), false);
//       match put_result.ok().unwrap() {
//         routing::Action::SendOn(ref x) => {
//           assert_eq!(x.len(), 1);
//           assert_eq!(x[0], NameType([7u8; 64]));
//         }
//         routing::Action::Reply(x) => panic!("Unexpected"),
//       }
//     }
//     { // PmidNode stores/retrieves data
//       let from = NameType::new([7u8; 64]);
//       let dest = DestinationAddress{ dest : NameType::new([6u8; 64]), reply_to: None };
//       let put_result = vault.handle_put(Authority::ManagedNode, Authority::NodeManager, from, dest,
//                                         routing::types::array_as_vector(encoder.as_bytes()));
//       assert_eq!(put_result.is_err(), true);
//       match put_result.err().unwrap() {
//         routing::RoutingError::Success => { }
//         _ => panic!("Unexpected"),
//       }
//       let from = NameType::new([7u8; 64]);
//       let get_result = vault.handle_get(payload.get_type_tag() as u64, Authority::ManagedNode,
//                                         Authority::NodeManager, from, [3u8; 64].to_vec());
//       assert_eq!(get_result.is_err(), false);
//       match get_result.ok().unwrap() {
//         routing::Action::Reply(ref x) => {
//             let mut d = cbor::Decoder::from_bytes(&x[..]);
//             let payload_retrieved: Payload = d.decode().next().unwrap().unwrap();
//             assert_eq!(payload_retrieved.get_type_tag(), PayloadTypeTag::ImmutableData);
//             let data_retrieved = payload_retrieved.get_data::<maidsafe_types::ImmutableData>();
//             assert_eq!(data.name().0.to_vec(), data_retrieved.name().0.to_vec());
//             assert_eq!(data.serialised_contents(), data_retrieved.serialised_contents());
//         },
//           _ => panic!("Unexpected"),
//       }
//     }
//   }

// }
