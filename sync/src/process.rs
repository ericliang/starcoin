use actix::prelude::*;
use actix::{
    fut::wrap_future, fut::FutureWrap, Actor, Addr, AsyncContext, Context, Handler,
    ResponseActFuture,
};
use anyhow::Result;
use bus::{Bus, BusActor, Subscription};
use chain::{ChainActor, ChainActorRef};
use crypto::{hash::CryptoHash, HashValue};
use futures_timer::Delay;
/// Sync message which inbound
use network::sync_messages::{
    BatchBodyMsg, BatchHashByNumberMsg, BatchHeaderMsg, BlockBody, DataType, DownloadMessage,
    GetDataByHashMsg, GetHashByNumberMsg, HashWithBlockHeader, HashWithNumber, LatestStateMsg,
    ProcessMessage,
};
use network::{
    NetworkAsyncService, PeerMessage, RPCMessage, RPCRequest, RPCResponse, RpcRequestMessage,
};
use std::hash::Hash;
use std::sync::Arc;
use std::time::Duration;
use traits::{AsyncChain, Chain, ChainAsyncService, ChainReader, ChainService};
use txpool::TxPoolRef;
use types::{block::Block, peer_info::PeerInfo};

pub struct ProcessActor {
    processor: Arc<Processor>,
    peer_info: Arc<PeerInfo>,
    network: NetworkAsyncService<TxPoolRef>,
    bus: Addr<BusActor>,
}

impl ProcessActor {
    pub fn launch(
        peer_info: Arc<PeerInfo>,
        chain_reader: ChainActorRef<ChainActor>,
        network: NetworkAsyncService<TxPoolRef>,
        bus: Addr<BusActor>,
    ) -> Result<Addr<ProcessActor>> {
        let process_actor = ProcessActor {
            processor: Arc::new(Processor::new(chain_reader)),
            peer_info,
            network,
            bus,
        };
        Ok(process_actor.start())
    }
}

impl Actor for ProcessActor {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        let rpc_recipient = ctx.address().recipient::<RpcRequestMessage>();
        self.bus
            .send(Subscription {
                recipient: rpc_recipient,
            })
            .into_actor(self)
            .then(|_res, act, _ctx| async {}.into_actor(act))
            .wait(ctx);
        info!("Process actor started");
    }
}

impl Handler<ProcessMessage> for ProcessActor {
    type Result = ResponseActFuture<Self, Result<()>>;

    fn handle(&mut self, msg: ProcessMessage, ctx: &mut Self::Context) -> Self::Result {
        let mut processor = self.processor.clone();
        let my_peer_info = self.peer_info.as_ref().clone();
        let network = self.network.clone();
        let fut = async move {
            let id = msg.crypto_hash();
            match msg {
                ProcessMessage::NewPeerMsg(peer_info) => {
                    debug!(
                        "send latest_state_msg to peer : {:?}:{:?}",
                        peer_info.id, my_peer_info.id
                    );
                    let latest_state_msg =
                        Processor::send_latest_state_msg(processor.clone()).await;
                    Delay::new(Duration::from_secs(1)).await;
                    network
                        .clone()
                        .send_peer_message(
                            peer_info.id,
                            PeerMessage::LatestStateMsg(latest_state_msg),
                        )
                        .await;
                }
                _ => {}
            }

            Ok(())
        };

        Box::new(wrap_future::<_, Self>(fut))
    }
}

impl Handler<RpcRequestMessage> for ProcessActor {
    type Result = Result<()>;

    fn handle(&mut self, msg: RpcRequestMessage, ctx: &mut Self::Context) -> Self::Result {
        let id = (&msg.request).get_id();
        let peer_id = (&msg).peer_id;
        let processor = self.processor.clone();
        let network = self.network.clone();
        match msg.request {
            RPCRequest::TestRequest(_r) => {}
            RPCRequest::GetHashByNumberMsg(process_msg)
            | RPCRequest::GetDataByHashMsg(process_msg) => match process_msg {
                ProcessMessage::GetHashByNumberMsg(get_hash_by_number_msg) => {
                    debug!("get_hash_by_number_msg");
                    Arbiter::spawn(async move {
                        let batch_hash_by_number_msg = Processor::handle_get_hash_by_number_msg(
                            id.clone(),
                            processor.clone(),
                            get_hash_by_number_msg,
                        )
                        .await;

                        let resp = RPCResponse::BatchHashByNumberMsg(batch_hash_by_number_msg);
                        network.clone().response_for(peer_id, id, resp).await;
                    });
                }
                ProcessMessage::GetDataByHashMsg(get_data_by_hash_msg) => {
                    Arbiter::spawn(async move {
                        match get_data_by_hash_msg.data_type {
                            DataType::HEADER => {
                                let batch_header_msg = Processor::handle_get_header_by_hash_msg(
                                    processor.clone(),
                                    get_data_by_hash_msg.clone(),
                                )
                                .await;
                                let batch_body_msg = Processor::handle_get_body_by_hash_msg(
                                    processor.clone(),
                                    get_data_by_hash_msg,
                                )
                                .await;
                                debug!(
                                    "batch block size: {} : {}",
                                    batch_header_msg.headers.len(),
                                    batch_body_msg.bodies.len()
                                );

                                let resp = RPCResponse::BatchHeaderAndBodyMsg(
                                    id,
                                    batch_header_msg,
                                    batch_body_msg,
                                );
                                network.clone().response_for(peer_id, id, resp).await;
                            }
                            _ => {}
                        }
                    });
                }
                ProcessMessage::NewPeerMsg(_) => unreachable!(),
            },
        }

        Ok(())
    }
}

/// Process request for syncing block
pub struct Processor {
    chain_reader: ChainActorRef<ChainActor>,
}

impl Processor {
    pub fn new(chain_reader: ChainActorRef<ChainActor>) -> Self {
        Processor { chain_reader }
    }

    pub async fn head_block(processor: Arc<Processor>) -> Block {
        processor.chain_reader.clone().head_block().await.unwrap()
    }

    pub async fn send_latest_state_msg(processor: Arc<Processor>) -> LatestStateMsg {
        let head_block = Self::head_block(processor.clone()).await;
        //todo:send to network
        let hash_header = HashWithBlockHeader {
            hash: head_block.crypto_hash(),
            header: head_block.header().clone(),
        };
        LatestStateMsg { hash_header }
    }

    pub async fn handle_get_hash_by_number_msg(
        req_id: HashValue,
        processor: Arc<Processor>,
        get_hash_by_number_msg: GetHashByNumberMsg,
    ) -> BatchHashByNumberMsg {
        let mut hashs = Vec::new();
        for number in get_hash_by_number_msg.numbers {
            let block = processor
                .chain_reader
                .clone()
                .get_block_by_number(number)
                .await
                .unwrap();
            debug!(
                "block number:{:?}, hash {:?}",
                block.header().number(),
                block.header().id()
            );
            let hash_with_number = HashWithNumber {
                number: block.header().number(),
                hash: block.header().id(),
            };

            hashs.push(hash_with_number);
        }

        BatchHashByNumberMsg { id: req_id, hashs }
    }

    pub async fn handle_get_header_by_hash_msg(
        processor: Arc<Processor>,
        get_header_by_hash_msg: GetDataByHashMsg,
    ) -> BatchHeaderMsg {
        let mut headers = Vec::new();
        for hash in get_header_by_hash_msg.hashs {
            let header = processor
                .chain_reader
                .clone()
                .get_header_by_hash(&hash)
                .await
                .unwrap();
            let header = HashWithBlockHeader { header, hash };

            headers.push(header);
        }
        BatchHeaderMsg { headers }
    }

    pub async fn handle_get_body_by_hash_msg(
        processor: Arc<Processor>,
        get_body_by_hash_msg: GetDataByHashMsg,
    ) -> BatchBodyMsg {
        let mut bodies = Vec::new();
        for hash in get_body_by_hash_msg.hashs {
            let transactions = match processor
                .chain_reader
                .clone()
                .get_block_by_hash(&hash)
                .await
            {
                Some(block) => block.transactions().clone().to_vec(),
                _ => Vec::new(),
            };

            let body = BlockBody { transactions, hash };

            bodies.push(body);
        }
        BatchBodyMsg { bodies }
    }
}
