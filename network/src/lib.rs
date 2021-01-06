// Copyright (C) 2019-2020 Aleo Systems Inc.
// This file is part of the snarkOS library.

// The snarkOS library is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// The snarkOS library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with the snarkOS library. If not, see <https://www.gnu.org/licenses/>.

// Compilation
#![allow(clippy::module_inception)]
#![warn(unused_extern_crates)]
#![forbid(unsafe_code)]
// Documentation
#![cfg_attr(nightly, feature(doc_cfg, external_doc))]
#![cfg_attr(nightly, doc(include = "../documentation/concepts/network_server.md"))]

#[macro_use]
extern crate thiserror;

#[macro_use]
extern crate tracing;
#[macro_use]
extern crate snarkos_metrics;

pub mod external;

pub mod blocks;
pub use blocks::*;

pub mod environment;
pub use environment::*;

pub mod errors;
pub use errors::*;

pub mod inbound;
pub use inbound::*;

pub mod outbound;
pub use outbound::*;

pub mod peers;
pub use peers::*;

use crate::{external::Channel, peers::peers::Peers};

use parking_lot::RwLock;
use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};
use tokio::{task, time::sleep};

pub(crate) type Sender = tokio::sync::mpsc::Sender<Response>;

pub(crate) type Receiver = tokio::sync::mpsc::Receiver<Response>;

/// A core data structure for operating the networking stack of this node.
#[derive(Clone)]
pub struct Server {
    /// The parameters and settings of this node server.
    pub environment: Environment,
    /// The inbound handler of this node server.
    inbound: Arc<Inbound>,
    /// The outbound handler of this node server.
    outbound: Arc<Outbound>,

    pub peers: Peers,
    pub blocks: Blocks,

    /// The current sync state, shared reference to allow updating from withing blocks.
    sync_state: Arc<RwLock<SyncState>>,
}

impl Server {
    /// Creates a new instance of `Server`.
    pub async fn new(environment: Environment) -> Result<Self, NetworkError> {
        let channels: Arc<RwLock<HashMap<SocketAddr, Channel>>> = Default::default();
        // Create the inbound and outbound handlers.
        let inbound = Arc::new(Inbound::new(channels.clone()));
        let outbound = Arc::new(Outbound::new(channels));

        // Initialize the peer and block services.
        let sync_state = Arc::new(RwLock::new(SyncState::Idle));
        let peers = Peers::new(environment.clone(), inbound.clone(), outbound.clone())?;
        let blocks = Blocks::new(environment.clone(), outbound.clone(), sync_state.clone())?;

        Ok(Self {
            environment,
            inbound,
            outbound,
            peers,
            blocks,
            sync_state,
        })
    }

    pub async fn establish_address(&mut self) -> Result<(), NetworkError> {
        self.inbound.listen(&mut self.environment).await?;
        let address = self.environment.local_address().unwrap();

        // update the local address for Blocks and Peers
        self.peers.environment.set_local_address(address);
        self.blocks.environment.set_local_address(address);

        Ok(())
    }

    pub async fn start_services(&self) -> Result<(), NetworkError> {
        let peers = self.peers.clone();
        let blocks = self.blocks.clone();
        let server = self.clone();
        let server_clone = self.clone();

        task::spawn(async move {
            loop {
                sleep(Duration::from_secs(10)).await;
                info!("Updating peers and blocks");
                let sync_node = peers.last_seen();
                if let Err(e) = peers.update().await {
                    error!("Peer update error: {}", e);
                }

                // sync only if sync isn't already in progress
                if let Err(e) = blocks.update(sync_node).await {
                    error!("Block update error: {}", e);
                }
            }
        });

        task::spawn(async move {
            loop {
                if let Err(e) = server_clone.receive_response().await {
                    error!("Server error: {}", e);
                }
            }
        });

        Ok(())
    }

    pub async fn start(&mut self) -> Result<(), NetworkError> {
        debug!("Initializing the connection server");
        self.establish_address().await?;
        self.start_services().await?;
        debug!("Connection server initialized");

        Ok(())
    }

    #[inline]
    pub fn local_address(&self) -> Option<SocketAddr> {
        self.environment.local_address()
    }

    async fn receive_response(&self) -> Result<(), NetworkError> {
        let response = self
            .inbound
            .receiver()
            .lock()
            .await
            .recv()
            .await
            .ok_or(NetworkError::ReceiverFailedToParse)?;

        match response {
            Response::ConnectingTo(remote_address, nonce) => {
                self.peers.connecting_to_peer(remote_address, nonce).await?;
            }
            Response::ConnectedTo(remote_address, nonce) => {
                self.peers.connected_to_peer(remote_address, nonce).await?;
            }
            Response::VersionToVerack(remote_address, remote_version) => {
                self.peers.version_to_verack(remote_address, &remote_version).await?;
            }
            Response::Verack(remote_address, verack) => {
                self.peers.verack(&remote_address, &verack).await?;
            }
            Response::Transaction(source, transaction) => {
                let connected_peers = self.peers.connected_peers();
                self.blocks
                    .received_transaction(source, transaction, connected_peers)
                    .await?;
            }
            Response::Block(remote_address, block, propagate) => {
                let connected_peers = match propagate {
                    true => Some(self.peers.connected_peers()),
                    false => None,
                };
                self.blocks
                    .received_block(remote_address, block, connected_peers)
                    .await?;
            }
            Response::GetBlock(remote_address, getblock) => {
                self.blocks.received_get_block(remote_address, getblock).await?;
            }
            Response::GetMemoryPool(remote_address) => {
                self.blocks.received_get_memory_pool(remote_address).await?;
            }
            Response::MemoryPool(mempool) => {
                self.blocks.received_memory_pool(mempool)?;
            }
            Response::GetSync(remote_address, getsync) => {
                self.blocks.received_get_sync(remote_address, getsync).await?;
            }
            Response::Sync(remote_address, sync) => {
                self.blocks.received_sync(remote_address, sync).await?;
            }
            Response::DisconnectFrom(remote_address) => {
                self.peers.disconnected_from_peer(&remote_address).await?;
            }
            Response::GetPeers(remote_address) => {
                self.peers.send_get_peers(remote_address).await?;
            }
            Response::Peers(_, peers) => {
                self.peers.process_inbound_peers(peers)?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external::{
        message::{read_header, read_message, Message, MessageHeader},
        message_name::MessageName,
        Block,
        GetBlock,
        GetMemoryPool,
        GetPeers,
        GetSync,
        MemoryPool,
        Peers,
        Sync,
        SyncBlock,
        Transaction,
        Verack,
        Version,
    };
    use snarkos_testing::{
        consensus::{BLOCK_1, BLOCK_2, DATA, FIXTURE_VK, GENESIS_BLOCK_HEADER_HASH, TEST_CONSENSUS},
        dpc::load_verifying_parameters,
    };
    use snarkvm_objects::block_header_hash::BlockHeaderHash;

    use std::{sync::Arc, time::Duration};

    use chrono::{DateTime, Utc};
    use parking_lot::{Mutex, RwLock};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };

    async fn test_node(bootnodes: Vec<String>) -> Server {
        let storage = FIXTURE_VK.ledger();
        let memory_pool = snarkos_consensus::MemoryPool::new();
        let memory_pool_lock = Arc::new(Mutex::new(memory_pool));
        let consensus = TEST_CONSENSUS.clone();
        let parameters = load_verifying_parameters();
        let socket_address = None;
        let min_peers = 1;
        let max_peers = 10;
        let sync_interval = 100;
        let mempool_interval = 5;
        let is_bootnode = false;
        let is_miner = false;

        let environment = Environment::new(
            Arc::new(RwLock::new(storage)),
            memory_pool_lock,
            Arc::new(consensus),
            Arc::new(parameters),
            socket_address,
            min_peers,
            max_peers,
            sync_interval,
            mempool_interval,
            bootnodes,
            is_bootnode,
            is_miner,
        )
        .unwrap();

        Server::new(environment).await.unwrap()
    }

    async fn write_message_to_stream(message_name: MessageName, message: impl Message, peer_stream: &mut TcpStream) {
        let serialized = message.serialize().unwrap();
        let header = MessageHeader::new(message_name, serialized.len() as u32)
            .serialize()
            .unwrap();
        peer_stream.write_all(&header).await.unwrap();
        peer_stream.write_all(&serialized).await.unwrap();
        peer_stream.flush().await.unwrap();
    }

    #[tokio::test]
    async fn starts_server() {
        let mut server = test_node(vec![]).await;
        assert!(server.start().await.is_ok());
        let address = server.local_address().unwrap();

        assert!(TcpListener::bind(address).await.is_err());
        assert_eq!(server.peers.number_of_connected_peers(), 0);
    }

    #[tokio::test]
    async fn handshake_responder_side() {
        // start a test node and listen for incoming connections
        let mut node = test_node(vec![]).await;
        node.start().await.unwrap();
        let node_listener = node.local_address().unwrap();

        // set up a fake node (peer), which is just a socket
        let mut peer_stream = TcpStream::connect(&node_listener).await.unwrap();

        // register the addresses bound to the connection between the node and the peer
        let peer_address = peer_stream.local_addr().unwrap();
        let node_address = peer_stream.peer_addr().unwrap();

        // the peer initiates a handshake by sending a Version message
        let version = Version::new(1u64, 1u32, 1u64, peer_address, node_address);
        write_message_to_stream(Version::name(), version, &mut peer_stream).await;

        // at this point the node should have marked the peer as ' connecting'
        sleep(Duration::from_millis(200)).await;
        assert!(node.peers.is_connecting(&peer_address));

        // check if the peer has received the Verack message from the node
        let header = read_header(&mut peer_stream).await.unwrap();
        let message = read_message(&mut peer_stream, header.len as usize).await.unwrap();
        let _verack = Verack::deserialize(&message).unwrap();

        // check if it was followed by a Version message
        let header = read_header(&mut peer_stream).await.unwrap();
        let message = read_message(&mut peer_stream, header.len as usize).await.unwrap();
        let version = Version::deserialize(&message).unwrap();

        // in response to the Version, the peer sends a Verack message to finish the handshake
        let verack = Verack::new(version.nonce, peer_address, node_address);
        write_message_to_stream(Verack::name(), verack, &mut peer_stream).await;

        // the node should now have register the peer as 'connected'
        sleep(Duration::from_millis(200)).await;
        assert!(node.peers.is_connected(&peer_address));
        assert_eq!(node.peers.number_of_connected_peers(), 1);
    }

    #[tokio::test]
    async fn handshake_initiator_side() {
        // start a fake peer which is just a socket
        let peer_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer_address = peer_listener.local_addr().unwrap();

        // start node with the peer as a bootnode; that way it will get connected to
        let mut node = test_node(vec![peer_address.to_string()]).await;
        node.start().await.unwrap();

        // accept the node's connection on peer side
        let (mut peer_stream, node_address) = peer_listener.accept().await.unwrap();

        // the peer should receive a Version message from the node (initiator of the handshake)
        let header = read_header(&mut peer_stream).await.unwrap();
        let message = read_message(&mut peer_stream, header.len as usize).await.unwrap();
        let version = Version::deserialize(&message).unwrap();

        // at this point the node should have marked the peer as 'connecting'
        assert!(node.peers.is_connecting(&peer_address));

        // the peer responds with a Verack acknowledging the Version message
        let verack = Verack::new(version.nonce, peer_address, node_address);
        write_message_to_stream(Verack::name(), verack, &mut peer_stream).await;

        // the peer then follows up with a Version message
        let version = Version::new(1u64, 1u32, 1u64, peer_address, node_address);
        write_message_to_stream(Version::name(), version, &mut peer_stream).await;

        // the node should now have registered the peer as 'connected'
        sleep(Duration::from_millis(200)).await;
        assert!(node.peers.is_connected(&peer_address));
        assert_eq!(node.peers.number_of_connected_peers(), 1);
    }

    async fn assert_node_rejected_message(node: &Server, peer_stream: &mut TcpStream) {
        // slight delay for server to process the message
        sleep(Duration::from_millis(200)).await;

        // read the response from the stream
        let mut buffer = String::new();
        let bytes_read = peer_stream.read_to_string(&mut buffer).await.unwrap();

        // check node's response is empty
        assert_eq!(bytes_read, 0);
        assert!(buffer.is_empty());

        // check the node's state hasn't been altered by the message
        assert!(!node.peers.is_connecting(&peer_stream.local_addr().unwrap()));
        assert_eq!(node.peers.number_of_connected_peers(), 0);
    }

    #[tokio::test]
    async fn reject_non_version_messages_before_handshake() {
        // start the node
        let mut node = test_node(vec![]).await;
        node.start().await.unwrap();

        // start the fake node (peer) which is just a socket
        // note: the connection needs to be re-established as it is reset
        let mut peer_stream = TcpStream::connect(node.local_address().unwrap()).await.unwrap();

        // send a GetPeers message without a prior handshake established
        write_message_to_stream(GetPeers::name(), GetPeers, &mut peer_stream).await;

        // verify the node rejected the message, the response to the peer is empty and the node's
        // state is unaltered
        assert_node_rejected_message(&node, &mut peer_stream).await;

        // GetMemoryPool
        let mut peer_stream = TcpStream::connect(node.local_address().unwrap()).await.unwrap();
        write_message_to_stream(GetMemoryPool::name(), GetMemoryPool, &mut peer_stream).await;
        assert_node_rejected_message(&node, &mut peer_stream).await;

        // GetBlock
        let mut peer_stream = TcpStream::connect(node.local_address().unwrap()).await.unwrap();
        let block_hash = BlockHeaderHash::new([0u8; 32].to_vec());
        write_message_to_stream(GetBlock::name(), GetBlock::new(block_hash), &mut peer_stream).await;
        assert_node_rejected_message(&node, &mut peer_stream).await;

        // GetSync
        let mut peer_stream = TcpStream::connect(node.local_address().unwrap()).await.unwrap();
        let block_hash = BlockHeaderHash::new([0u8; 32].to_vec());
        write_message_to_stream(GetSync::name(), GetSync::new(vec![block_hash]), &mut peer_stream).await;
        assert_node_rejected_message(&node, &mut peer_stream).await;

        // Peers
        let mut peer_stream = TcpStream::connect(node.local_address().unwrap()).await.unwrap();
        let peers = Peers::new(vec![("127.0.0.1:0".parse().unwrap(), Utc::now())]);
        write_message_to_stream(Peers::name(), peers, &mut peer_stream).await;
        assert_node_rejected_message(&node, &mut peer_stream).await;

        // MemoryPool
        let mut peer_stream = TcpStream::connect(node.local_address().unwrap()).await.unwrap();
        let memory_pool = MemoryPool::new(vec![[0u8, 10].to_vec()]);
        write_message_to_stream(MemoryPool::name(), memory_pool, &mut peer_stream).await;
        assert_node_rejected_message(&node, &mut peer_stream).await;

        // Block
        let mut peer_stream = TcpStream::connect(node.local_address().unwrap()).await.unwrap();
        let block = Block::new([0u8, 10].to_vec());
        write_message_to_stream(Block::name(), block, &mut peer_stream).await;
        assert_node_rejected_message(&node, &mut peer_stream).await;

        // SyncBlock
        let mut peer_stream = TcpStream::connect(node.local_address().unwrap()).await.unwrap();
        let sync_block = SyncBlock::new([0u8, 10].to_vec());
        write_message_to_stream(SyncBlock::name(), sync_block, &mut peer_stream).await;
        assert_node_rejected_message(&node, &mut peer_stream).await;

        // Sync
        let mut peer_stream = TcpStream::connect(node.local_address().unwrap()).await.unwrap();
        let block_hash = BlockHeaderHash::new([0u8; 32].to_vec());
        write_message_to_stream(Sync::name(), Sync::new(vec![block_hash]), &mut peer_stream).await;
        assert_node_rejected_message(&node, &mut peer_stream).await;

        // Transaction
        let mut peer_stream = TcpStream::connect(node.local_address().unwrap()).await.unwrap();
        let transaction = Transaction::new([0u8, 10].to_vec());
        write_message_to_stream(Transaction::name(), transaction, &mut peer_stream).await;
        assert_node_rejected_message(&node, &mut peer_stream).await;

        // Verack
        let mut peer_stream = TcpStream::connect(node.local_address().unwrap()).await.unwrap();
        let verack = Verack::new(1u64, peer_stream.local_addr().unwrap(), node.local_address().unwrap());
        write_message_to_stream(Verack::name(), verack, &mut peer_stream).await;
        assert_node_rejected_message(&node, &mut peer_stream).await;
    }

    // Unit test for block syncing?
    //
    // Tests:
    //
    // 1. Sync initiator side
    // 2. Sync responder side

    async fn handshake() -> (Server, TcpStream) {
        // start a test node and listen for incoming connections
        let mut node = test_node(vec![]).await;
        node.start().await.unwrap();
        let node_listener = node.local_address().unwrap();

        // set up a fake node (peer), which is just a socket
        let mut peer_stream = TcpStream::connect(&node_listener).await.unwrap();

        // register the addresses bound to the connection between the node and the peer
        let peer_address = peer_stream.local_addr().unwrap();
        let node_address = peer_stream.peer_addr().unwrap();

        // the peer initiates a handshake by sending a Version message
        let version = Version::new(1u64, 1u32, 1u64, peer_address, node_address);
        write_message_to_stream(Version::name(), version, &mut peer_stream).await;

        // at this point the node should have marked the peer as ' connecting'
        sleep(Duration::from_millis(200)).await;
        assert!(node.peers.is_connecting(&peer_address));

        // check if the peer has received the Verack message from the node
        let header = read_header(&mut peer_stream).await.unwrap();
        let message = read_message(&mut peer_stream, header.len as usize).await.unwrap();
        let _verack = Verack::deserialize(&message).unwrap();

        // check if it was followed by a Version message
        let header = read_header(&mut peer_stream).await.unwrap();
        let message = read_message(&mut peer_stream, header.len as usize).await.unwrap();
        let version = Version::deserialize(&message).unwrap();

        // in response to the Version, the peer sends a Verack message to finish the handshake
        let verack = Verack::new(version.nonce, peer_address, node_address);
        write_message_to_stream(Verack::name(), verack, &mut peer_stream).await;

        // the node should now have register the peer as 'connected'
        sleep(Duration::from_millis(200)).await;
        assert!(node.peers.is_connected(&peer_address));

        (node, peer_stream)
    }

    #[tokio::test]
    async fn sync_initiator_side() {
        // 1. Start server
        // 2. Start fake node
        // 3. Handshake (untested) maybe this should be setup code?
        //
        // 4. Expect GetSync
        // 5. Respond with Sync
        //
        // 6. Expect GetBlock
        // 7. Respond with Block
        //
        // 8. Somehow inspect state change? Environment.storage() and Environment.memory_pool()

        let filter =
            tracing_subscriber::EnvFilter::from_default_env().add_directive("tokio_reactor=off".parse().unwrap());
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .init();

        let (mut node, mut peer_stream) = handshake().await;

        // check Version from peer update is received
        let header = read_header(&mut peer_stream).await.unwrap();
        let message = read_message(&mut peer_stream, header.len as usize).await.unwrap();
        let version = Version::deserialize(&message).unwrap();

        // check GetSync message was received
        let header = read_header(&mut peer_stream).await.unwrap();
        let message = read_message(&mut peer_stream, header.len as usize).await.unwrap();
        let get_sync = GetSync::deserialize(&message).unwrap();

        // TODO: check block locator hashes
        // The node should have a block height of 0 here, i.e. only the genesis block is stored in
        // the ledger. The block locator hash should reflect this and means the fake node should
        // respond with the blocks following the genesis block.
        //
        // The genesis block is not being included in get_block_locator_hashes when it's the only
        // block in the ledger.
        // assert_eq!(
        //     get_sync.block_locator_hashes.first().unwrap().0,
        //     *GENESIS_BLOCK_HEADER_HASH
        // );

        let block_1_header_hash = BlockHeaderHash::new(DATA.block_1.header.get_hash().0.to_vec());
        let block_2_header_hash = BlockHeaderHash::new(DATA.block_2.header.get_hash().0.to_vec());

        let block_header_hashes = vec![block_1_header_hash.clone(), block_2_header_hash.clone()];

        let sync = Sync::new(block_header_hashes);
        write_message_to_stream(Sync::name(), sync, &mut peer_stream).await;

        // make sure both GetBlock messages are received
        let header = read_header(&mut peer_stream).await.unwrap();
        let message = read_message(&mut peer_stream, header.len as usize).await.unwrap();
        let get_block = GetBlock::deserialize(&message).unwrap();

        assert_eq!(get_block.block_hash, block_1_header_hash);

        let header = read_header(&mut peer_stream).await.unwrap();
        let message = read_message(&mut peer_stream, header.len as usize).await.unwrap();
        let get_block = GetBlock::deserialize(&message).unwrap();

        assert_eq!(get_block.block_hash, block_2_header_hash);

        // respond with the Block
        let block_1 = Block::new(BLOCK_1.to_vec());
        write_message_to_stream(Block::name(), block_1, &mut peer_stream).await;

        let block_2 = Block::new(BLOCK_2.to_vec());
        write_message_to_stream(Block::name(), block_2, &mut peer_stream).await;

        sleep(Duration::from_millis(200)).await;

        // Check blocks are stored correctly
        assert!(
            node.environment
                .storage()
                .read()
                .block_hash_exists(&block_1_header_hash)
        );

        assert!(
            node.environment
                .storage()
                .read()
                .block_hash_exists(&block_2_header_hash)
        );
    }

    #[tokio::test]
    async fn sync_responder_side() {
        unimplemented!()
    }
}
