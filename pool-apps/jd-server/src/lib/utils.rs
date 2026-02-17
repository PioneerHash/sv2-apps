//! ## Utility types for JD-Server
//!
//! Contains helper types that were previously in roles-logic-sv2 but have been
//! removed from stratum-core main branch.

use stratum_apps::stratum_core::{
    binary_sv2::U256,
    bitcoin::{
        blockdata::block::{Header, Version},
        consensus,
        hash_types::BlockHash,
        hashes::{sha256d::Hash as DHash, Hash},
        Block, CompactTarget, Transaction,
    },
    job_declaration_sv2::{DeclareMiningJob, PushSolution},
};

/// Generator of unique IDs for channels and groups.
///
/// It keeps an internal counter, which is incremented every time a new unique id is requested.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Id {
    state: u32,
}

impl Id {
    /// Creates a new [`Id`] instance initialized to `0`.
    pub fn new() -> Self {
        Self { state: 0 }
    }

    /// Increments then returns the internal state on a new ID.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> u32 {
        self.state += 1;
        self.state
    }
}

impl Default for Id {
    fn default() -> Self {
        Self::new()
    }
}

/// Converts a `u256` to a [`BlockHash`] type.
pub fn u256_to_block_hash(v: U256<'static>) -> BlockHash {
    let hash: [u8; 32] = v.to_vec().try_into().unwrap();
    let hash = Hash::from_slice(&hash).unwrap();
    BlockHash::from_raw_hash(hash)
}

/// Computes the Merkle root from coinbase transaction components and a path of transaction hashes.
pub fn merkle_root_from_path<T: AsRef<[u8]>>(
    coinbase_tx_prefix: &[u8],
    coinbase_tx_suffix: &[u8],
    extranonce: &[u8],
    path: &[T],
) -> Option<Vec<u8>> {
    let mut coinbase =
        Vec::with_capacity(coinbase_tx_prefix.len() + coinbase_tx_suffix.len() + extranonce.len());
    coinbase.extend_from_slice(coinbase_tx_prefix);
    coinbase.extend_from_slice(extranonce);
    coinbase.extend_from_slice(coinbase_tx_suffix);
    let coinbase: Transaction = match consensus::deserialize(&coinbase[..]) {
        Ok(trans) => trans,
        Err(e) => {
            tracing::error!("ERROR: {}", e);
            return None;
        }
    };

    let coinbase_id: [u8; 32] = *coinbase.compute_txid().as_ref();

    Some(merkle_root_from_path_(coinbase_id, path).to_vec())
}

/// Computes the Merkle root from a validated coinbase transaction and a path of transaction
/// hashes.
pub fn merkle_root_from_path_<T: AsRef<[u8]>>(coinbase_id: [u8; 32], path: &[T]) -> [u8; 32] {
    match path.len() {
        0 => coinbase_id,
        _ => reduce_path(coinbase_id, path),
    }
}

// Computes the Merkle root by iteratively combining the coinbase transaction hash with each
// transaction hash in the `path`.
fn reduce_path<T: AsRef<[u8]>>(coinbase_id: [u8; 32], path: &[T]) -> [u8; 32] {
    let mut root = coinbase_id;
    for node in path {
        let to_hash = [&root[..], node.as_ref()].concat();
        let hash = DHash::hash(&to_hash);
        root = *hash.as_ref();
    }
    root
}

/// Creates a block from a solution submission.
///
/// Facilitates the creation of valid Bitcoin blocks by combining a declared mining job, a list of
/// transactions, and a solution message from the mining device.
pub struct BlockCreator<'a> {
    last_declare: DeclareMiningJob<'a>,
    tx_list: Vec<Transaction>,
    message: PushSolution<'a>,
}

impl<'a> BlockCreator<'a> {
    /// Creates a new [`BlockCreator`] instance.
    pub fn new(
        last_declare: DeclareMiningJob<'a>,
        tx_list: Vec<Transaction>,
        message: PushSolution<'a>,
    ) -> BlockCreator<'a> {
        BlockCreator {
            last_declare,
            tx_list,
            message,
        }
    }
}

impl<'a> From<BlockCreator<'a>> for Block {
    fn from(block_creator: BlockCreator<'a>) -> Block {
        let last_declare = block_creator.last_declare;
        let mut tx_list = block_creator.tx_list;
        let message = block_creator.message;

        let coinbase_pre = last_declare.coinbase_tx_prefix.to_vec();
        let extranonce = message.extranonce.to_vec();
        let coinbase_suf = last_declare.coinbase_tx_suffix.to_vec();
        let mut path: Vec<Vec<u8>> = vec![];
        for tx in &tx_list {
            let id = tx.compute_txid();
            let id_bytes: &[u8; 32] = id.as_ref();
            path.push(id_bytes.to_vec());
        }
        let merkle_root =
            merkle_root_from_path(&coinbase_pre[..], &coinbase_suf[..], &extranonce[..], &path)
                .expect("Invalid coinbase");
        let merkle_root = Hash::from_slice(merkle_root.as_slice()).unwrap();

        let prev_blockhash = u256_to_block_hash(message.prev_hash.into_static());
        let header = Header {
            version: Version::from_consensus(message.version as i32),
            prev_blockhash,
            merkle_root,
            time: message.ntime,
            bits: CompactTarget::from_consensus(message.nbits),
            nonce: message.nonce,
        };

        let coinbase = [coinbase_pre, extranonce, coinbase_suf].concat();
        let coinbase = consensus::deserialize(&coinbase[..]).unwrap();
        tx_list.insert(0, coinbase);

        let mut block = Block {
            header,
            txdata: tx_list.clone(),
        };

        block.header.merkle_root = block.compute_merkle_root().unwrap();
        block
    }
}
