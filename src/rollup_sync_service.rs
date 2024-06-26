use std::{
    cmp::{max, min},
    env,
    fs::File,
    io::{Error, Read},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use alloy_primitives::Address;
use alloy_rlp::RlpEncodable;
use alloy_sol_types::{private::FixedBytes, sol, SolEventInterface, SolInterface};
use ethers::{abi::Abi, types::H256, utils::rlp};
use ethers::{
    providers::{Http, Middleware, Provider},
    types::H160,
};
use reth_db::{
    database::Database,
    mdbx::{
        tx::{self, Tx},
        RW,
    },
    table, tables,
    transaction::{DbTx, DbTxMut},
    DatabaseEnv,
};
use rlp::{Decodable, Encodable, Rlp, RlpStream};
use serde_json::Value;
use tokio::{sync::mpsc, time::sleep};

use crate::{
    rollup_sync_service_util::{decode_chunk_block_ranges, ChunkBlockRange},
    ScrollChain::ScrollChainEvents,
};
use dotenv::dotenv;

#[derive(Debug)]
pub struct CommitBatchArgs {
    version: u8,
    parent_batch_header: Vec<u8>,
    chunks: Vec<Vec<u8>>,
    skipped_l1_message_bitmap: Vec<u8>,
}

#[derive(Debug)]
pub struct RollupSyncService {
    db: Arc<DatabaseEnv>,
    last_synced_block: Option<u64>,
    provider: Provider<Http>,
    scroll_chain_abi: Abi,
}

impl RollupSyncService {
    pub fn new(db: Arc<DatabaseEnv>, provider: Provider<Http>) -> Self {
        let tx = db.tx_mut().expect("Could not create transaction");

        let mut last_processed_block = tx
            .get::<tables::RollupSyncL1LastBlockNumber>(
                "RLastRollupEventSyncedL1BlockNumber".to_string(),
            )
            .expect("Could not fetch last synced block number");
        println!("Last processed block: {:?}", last_processed_block);
        if last_processed_block.is_none() {
            last_processed_block = Some(19972300);
            tx.put::<tables::RollupSyncL1LastBlockNumber>(
                "RLastRollupEventSyncedL1BlockNumber".to_string(),
                last_processed_block.unwrap(),
            )
            .expect("Could not update last synced block number");
        }

        let scroll_chain_abi: Abi = get_abi("scroll_chain_abi.json");
        Self {
            db,
            last_synced_block: last_processed_block,
            provider,
            scroll_chain_abi,
        }
    }

    pub async fn start(&self, terminate_rx: &mut mpsc::Receiver<()>) {
        println!("Rollup sync service started");
        let mut tx = self.db.tx_mut().unwrap();
        loop {
            tokio::select! {
                _ = self.fetch_rollup_events(&mut tx) => {
                    break;
                }
                _ = terminate_rx.recv() => {
                    println!("Received a message to stop the Rollup sync service");
                    break;
                }
            }
        }
        tx.commit().expect("Could not commit transaction");
        println!("Rollup sync service stopped");
    }

    pub async fn fetch_rollup_events(&self, tx_mut: &mut Tx<RW>) {
        let from = self.last_synced_block.unwrap();
        let to = self.provider.get_block_number().await.unwrap().as_u64();
        println!(
            "-------------------Fetching rollup events from {} to {}",
            from, to
        );

        for block in (from..to).step_by(100) {
            let (filtered_rollup_events, last_block_number) = self
                .fetch_rollup_events_in_range(block, min(block + 100, to))
                .await;
            println!(
                "Fetched {} rollup events from block {} to {}",
                filtered_rollup_events.len(),
                block,
                last_block_number
            );

            // self.last_synced_block = Some(last_block_number);
            tx_mut
                .put::<tables::RollupSyncL1LastBlockNumber>(
                    "RLastRollupEventSyncedL1BlockNumber".to_string(),
                    last_block_number,
                )
                .expect("Could not update last synced block number");

            self.parse_and_update_rollup_events(filtered_rollup_events, tx_mut)
                .await;
        }
    }

    pub async fn fetch_rollup_events_in_range(
        &self,
        from: u64,
        to: u64,
    ) -> (Vec<(ScrollChainEvents, H256)>, u64) {
        let mut filtered_rollup_events = vec![];
        let l1_scroll_chain_address = env::var("L1_SCROLL_CHAIN").unwrap();
        println!("Fetching logs from {} to {}", from, to);
        for block_number in (from..to + 1).step_by(1) {
            println!("Block number: {:?}\n", block_number);
            let receipts = self.provider.get_block_receipts(block_number).await;
            if receipts.is_err() {
                println!(
                    "Error fetching receipts for block number: {:?}",
                    block_number
                );

                return (filtered_rollup_events, block_number - 1);
            }
            let receipts = receipts.unwrap();

            println!(
                "block number: {:?},, receipts length: {:?}",
                block_number,
                receipts.len()
            );

            let new_logs: Vec<(ScrollChainEvents, H256)> = receipts
                .iter()
                .filter(|receipt| {
                    receipt.to == Some(H160::from_str(&l1_scroll_chain_address).unwrap())
                })
                .flat_map(|receipt| receipt.logs.iter().map(move |log| (receipt, log)))
                .filter_map(|(receipt, log)| {
                    let topics: Vec<_> = log
                        .topics
                        .iter()
                        .map(|topic| FixedBytes::new(topic.to_fixed_bytes()))
                        .collect();
                    ScrollChainEvents::decode_raw_log(&topics, &log.data, true)
                        .ok()
                        .map(|event| (event, receipt.transaction_hash))
                })
                .filter(|(event, _)| match event {
                    ScrollChainEvents::CommitBatch(_) => true,
                    ScrollChainEvents::RevertBatch(_) => true,
                    ScrollChainEvents::FinalizeBatch(_) => true,
                    _ => false,
                })
                .collect();

            if new_logs.len() > 0 {
                println!(
                    "New logs: {:?}, block_number {:?}",
                    new_logs.len(),
                    block_number
                );
                filtered_rollup_events.extend(new_logs);
            }
        }

        return (filtered_rollup_events, to);
    }

    pub async fn parse_and_update_rollup_events(
        &self,
        rollup_events: Vec<(ScrollChainEvents, H256)>,
        tx_mut: &mut Tx<RW>,
    ) {
        for (event, tx_hash) in rollup_events {
            match event {
                ScrollChainEvents::CommitBatch(commit_batch) => {
                    let batch_index = commit_batch.batchIndex;

                    // convert batch_index to u64
                    let batch_index = batch_index.to::<u64>();

                    let chunk_block_ranges = self.get_chunk_ranges(tx_hash).await;

                    println!(
                        "Length of chunk_block_ranges: {:?}",
                        chunk_block_ranges.len()
                    );
                    // encode chunk_block_ranges
                    let mut stream = RlpStream::new_list(chunk_block_ranges.len());
                    for chunk_range in chunk_block_ranges {
                        stream.append(&chunk_range);
                    }

                    let encoded_chunk_block_ranges = stream.out();
                    tx_mut
                        .put::<tables::RollupSyncL1BatchChunkRanges>(
                            format!("R-bcr{}", batch_index),
                            encoded_chunk_block_ranges.to_vec(),
                        )
                        .unwrap();
                }

                _ => {}
            }
        }
    }

    pub async fn get_chunk_ranges(&self, tx_hash: H256) -> Vec<ChunkBlockRange> {
        let transaction = self
            .provider
            .get_transaction(tx_hash)
            .await
            .unwrap()
            .unwrap();

        let tx_data = transaction.input.to_vec();
        let abi = self.scroll_chain_abi.clone();
        let chunk_block_ranges = decode_chunk_block_ranges(tx_data, &abi).unwrap();
        chunk_block_ranges
    }
}

pub fn get_abi(path: &str) -> Abi {
    let mut file = File::open(path).unwrap();

    let mut abi_json = String::new();
    file.read_to_string(&mut abi_json).unwrap();

    let parsed_json: Value = serde_json::from_str(&abi_json).unwrap();

    let abi_array = parsed_json["abi"].clone();
    // Parse the ABI
    serde_json::from_value(abi_array).unwrap()
}
