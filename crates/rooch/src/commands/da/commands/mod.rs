// Copyright (c) RoochNetwork
// SPDX-License-Identifier: Apache-2.0

use metrics::RegistryService;
use moveos_store::transaction_store::{TransactionDBStore, TransactionStore};
use moveos_types::h256::H256;
use moveos_types::moveos_std::object::ObjectMeta;
use moveos_types::transaction::TransactionExecutionInfo;
use rooch_config::RoochOpt;
use rooch_db::RoochDB;
use rooch_types::da::chunk::chunk_from_segments;
use rooch_types::da::segment::{segment_from_bytes, SegmentID};
use rooch_types::rooch_network::RoochChainID;
use rooch_types::transaction::LedgerTransaction;
use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;

pub mod dump_tx_order_hash;
pub mod exec;
pub mod namespace;
pub mod unpack;

// collect all the chunks from segment_dir.
// each segment is stored in a file named by the segment_id.
// each chunk may contain multiple segments.
// we collect all the chunks and their segment numbers to unpack them later.
pub(crate) fn collect_chunks(
    segment_dir: PathBuf,
) -> anyhow::Result<(HashMap<u128, Vec<u64>>, u128, u128)> {
    let mut chunks = HashMap::new();
    let mut max_chunk_id = 0;
    let mut min_chunk_id = u128::MAX;
    for entry in fs::read_dir(segment_dir.clone())?.flatten() {
        let path = entry.path();
        if path.is_file() {
            if let Some(segment_id) = path
                .file_name()
                .and_then(|s| s.to_str()?.parse::<SegmentID>().ok())
            {
                let chunk_id = segment_id.chunk_id;
                let segment_number = segment_id.segment_number;
                let segments: &mut Vec<u64> = chunks.entry(chunk_id).or_default();
                segments.push(segment_number);
                if chunk_id > max_chunk_id {
                    max_chunk_id = chunk_id;
                }
                if chunk_id < min_chunk_id {
                    min_chunk_id = chunk_id;
                }
            }
        }
    }
    if chunks.is_empty() {
        return Err(anyhow::anyhow!("No segment found in {:?}", segment_dir));
    }
    Ok((chunks, min_chunk_id, max_chunk_id))
}

pub(crate) fn get_tx_list_from_chunk(
    segment_dir: PathBuf,
    chunk_id: u128,
    segment_numbers: Vec<u64>,
) -> anyhow::Result<Vec<LedgerTransaction>> {
    let mut segments = Vec::new();
    for segment_number in segment_numbers {
        let segment_id = SegmentID {
            chunk_id,
            segment_number,
        };
        let segment_path = segment_dir.join(segment_id.to_string());
        let segment_bytes = fs::read(segment_path)?;
        let segment = segment_from_bytes(&segment_bytes)?;
        segments.push(segment);
    }
    let chunk = chunk_from_segments(segments)?;
    let batch = chunk.get_batches().into_iter().next().unwrap();
    batch.verify(true)?;
    Ok(batch.get_tx_list())
}

pub(crate) fn build_rooch_db(
    base_data_dir: Option<PathBuf>,
    chain_id: Option<RoochChainID>,
    enable_rocks_stats: bool,
) -> (ObjectMeta, RoochDB) {
    let mut opt = RoochOpt::new_with_default(base_data_dir, chain_id, None).unwrap();
    opt.store.enable_statistics = enable_rocks_stats;
    let registry_service = RegistryService::default();
    let rooch_db = RoochDB::init(opt.store_config(), &registry_service.default_registry()).unwrap();
    let root = rooch_db.latest_root().unwrap().unwrap();
    (root, rooch_db)
}

pub struct LedgerTxGetter {
    segment_dir: PathBuf,
    chunks: HashMap<u128, Vec<u64>>,
    min_chunk_id: u128,
    max_chunk_id: u128,
}

impl LedgerTxGetter {
    pub fn new(segment_dir: PathBuf) -> anyhow::Result<Self> {
        let (chunks, min_chunk_id, max_chunk_id) = collect_chunks(segment_dir.clone())?;

        Ok(LedgerTxGetter {
            segment_dir,
            chunks,
            min_chunk_id,
            max_chunk_id,
        })
    }

    pub fn load_ledger_tx_list(
        &self,
        chunk_id: u128,
        must_has: bool,
    ) -> anyhow::Result<Option<Vec<LedgerTransaction>>> {
        let segments = self.chunks.get(&chunk_id);
        if segments.is_none() {
            if must_has {
                return Err(anyhow::anyhow!("No segment found in chunk {}", chunk_id));
            }
            return Ok(None);
        }
        let tx_list = get_tx_list_from_chunk(
            self.segment_dir.clone(),
            chunk_id,
            segments.unwrap().clone(),
        )?;
        Ok(Some(tx_list))
    }

    pub fn get_max_chunk_id(&self) -> u128 {
        self.max_chunk_id
    }

    pub fn get_min_chunk_id(&self) -> u128 {
        self.min_chunk_id
    }
}

#[derive(Debug, Clone)]
pub struct TxOrderHashBlock {
    pub tx_order: u64,
    pub tx_hash: H256,
    pub block_number: u128,
}

impl TxOrderHashBlock {
    pub fn new(tx_order: u64, tx_hash: H256, block_number: u128) -> Self {
        TxOrderHashBlock {
            tx_order,
            tx_hash,
            block_number,
        }
    }
}

impl std::fmt::Display for TxOrderHashBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{:?}:{}",
            self.tx_order, self.tx_hash, self.block_number
        )
    }
}

impl std::str::FromStr for TxOrderHashBlock {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() != 3 {
            return Err(anyhow::anyhow!("Invalid format"));
        }
        let tx_order = parts[0].parse::<u64>()?;
        let tx_hash = H256::from_str(parts[1])?;
        let block_number = parts[2].parse::<u128>()?;
        Ok(TxOrderHashBlock {
            tx_order,
            tx_hash,
            block_number,
        })
    }
}

pub struct TxOrderHashBlockGetter {
    tx_order_hash_blocks: Vec<TxOrderHashBlock>,
    transaction_store: TransactionDBStore,
}

impl TxOrderHashBlockGetter {
    pub fn load_from_file(
        file_path: PathBuf,
        transaction_store: TransactionDBStore,
    ) -> anyhow::Result<Self> {
        let mut tx_order_hashes = Vec::with_capacity(70000000);
        let mut reader = BufReader::new(File::open(file_path)?);
        for line in reader.by_ref().lines() {
            let line = line?;
            let item = line.parse::<TxOrderHashBlock>()?;
            tx_order_hashes.push(item);
        }
        Ok(TxOrderHashBlockGetter {
            tx_order_hash_blocks: tx_order_hashes,
            transaction_store,
        })
    }

    pub fn slice(
        &self,
        start_tx_order: u64,
        end_tx_order: u64,
    ) -> anyhow::Result<Vec<TxOrderHashBlock>> {
        let r = self
            .tx_order_hash_blocks
            .binary_search_by(|x| x.tx_order.cmp(&start_tx_order));
        let start_idx = match r {
            Ok(i) => i,
            Err(_) => {
                return Err(anyhow::anyhow!("start_tx_order not found"));
            }
        };
        let end_idx = start_idx + (end_tx_order - start_tx_order) as usize;
        Ok(self.tx_order_hash_blocks[start_idx..end_idx + 1].to_vec())
    }

    pub fn find_last_executed(&self) -> anyhow::Result<Option<TxOrderHashBlock>> {
        // Check for an empty list
        if self.tx_order_hash_blocks.is_empty() {
            return Ok(None);
        }

        // Binary search
        let mut left = 0;
        let mut right = self.tx_order_hash_blocks.len() - 1;
        while left < right {
            let mid = (left + right) / 2;
            let tx_order_hash_block = &self.tx_order_hash_blocks[mid];
            let executed = self.has_executed(tx_order_hash_block.tx_hash)?;
            if executed {
                left = mid + 1;
            } else {
                right = mid;
            }
        }

        // Determine result
        let last_executed = self.has_executed(self.tx_order_hash_blocks[left].tx_hash)?;
        if left == 0 && !last_executed {
            return Ok(None);
        }
        if !last_executed {
            Ok(Some(self.tx_order_hash_blocks[left - 1].clone()))
        } else {
            Ok(Some(self.tx_order_hash_blocks[left].clone()))
        }
    }

    pub fn has_executed(&self, tx_hash: H256) -> anyhow::Result<bool> {
        let execution_info = self.transaction_store.get_tx_execution_info(tx_hash)?;
        Ok(execution_info.is_some())
    }

    pub fn get_execution_info(
        &self,
        tx_hash: H256,
    ) -> anyhow::Result<Option<TransactionExecutionInfo>> {
        let execution_info = self.transaction_store.get_tx_execution_info(tx_hash)?;
        Ok(execution_info)
    }
}
