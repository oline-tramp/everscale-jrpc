use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use arc_swap::ArcSwapOption;
use parking_lot::RwLock;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use ton_block::{Deserializable, HashmapAugType};
use ton_indexer::utils::{RefMcStateHandle, ShardStateStuff};
use ton_types::HashmapType;
use weedb::{rocksdb, Caches, Migrations, Semver, Table, WeeDb};

pub mod tables;

pub struct RuntimeStorage {
    key_block: watch::Sender<Option<ton_block::Block>>,
    masterchain_accounts_cache: RwLock<Option<ShardAccounts>>,
    shard_accounts_cache: RwLock<FxHashMap<ton_block::ShardIdent, ShardAccounts>>,
}

impl Default for RuntimeStorage {
    fn default() -> Self {
        let (key_block, _) = watch::channel(None);
        Self {
            key_block,
            masterchain_accounts_cache: Default::default(),
            shard_accounts_cache: Default::default(),
        }
    }
}

impl RuntimeStorage {
    pub fn subscribe_to_key_blocks(&self) -> watch::Receiver<Option<ton_block::Block>> {
        self.key_block.subscribe()
    }

    pub fn update_key_block(&self, block: &ton_block::Block) {
        self.key_block.send_replace(Some(block.clone()));
    }

    pub fn update_contract_states(
        &self,
        block_id: &ton_block::BlockIdExt,
        block_info: &ton_block::BlockInfo,
        shard_state: &ShardStateStuff,
    ) -> Result<()> {
        let accounts = shard_state.state().read_accounts()?;
        let state_handle = shard_state.ref_mc_state_handle().clone();

        let shard_accounts = ShardAccounts {
            accounts,
            state_handle,
            gen_utime: block_info.gen_utime().as_u32(),
        };

        if block_id.shard_id.is_masterchain() {
            *self.masterchain_accounts_cache.write() = Some(shard_accounts);
        } else {
            let mut cache = self.shard_accounts_cache.write();

            cache.insert(*block_info.shard(), shard_accounts);
            if block_info.after_merge() || block_info.after_split() {
                tracing::debug!("Clearing shard states cache after shards merge/split");

                let block_ids = block_info.read_prev_ids()?;
                match block_ids.len() {
                    // Block after split
                    //       |
                    //       *  - block A
                    //      / \
                    //     *   *  - blocks B', B"
                    1 => {
                        // Find all split shards for the block A
                        let (left, right) = block_ids[0].shard_id.split()?;

                        // Remove parent shard of the block A
                        if cache.contains_key(&left) && cache.contains_key(&right) {
                            cache.remove(&block_ids[0].shard_id);
                        }
                    }

                    // Block after merge
                    //     *   *  - blocks A', A"
                    //      \ /
                    //       *  - block B
                    //       |
                    2 => {
                        // Find and remove all parent shards
                        for block_id in block_info.read_prev_ids()? {
                            cache.remove(&block_id.shard_id);
                        }
                    }
                    _ => {}
                }
            }
        }

        Ok(())
    }

    pub fn get_contract_state(
        &self,
        account: &ton_block::MsgAddressInt,
    ) -> Result<ShardAccountFromCache> {
        let is_masterchain = account.is_masterchain();
        let account = account.address().get_bytestring_on_stack(0);
        let account = ton_types::UInt256::from_slice(account.as_slice());

        Ok(if is_masterchain {
            let state = self.masterchain_accounts_cache.read();
            match &*state {
                None => ShardAccountFromCache::NotReady,
                Some(accounts) => accounts
                    .get(&account)?
                    .map(ShardAccountFromCache::Found)
                    .unwrap_or(ShardAccountFromCache::NotFound),
            }
        } else {
            let cache = self.shard_accounts_cache.read();
            let mut state = Ok(None);

            let mut has_account_shard = false;
            for (shard_ident, shard_accounts) in cache.iter() {
                if !contains_account(shard_ident, &account) {
                    continue;
                }

                has_account_shard = true;
                state = shard_accounts.get(&account)
            }

            if !has_account_shard {
                return Ok(ShardAccountFromCache::NotReady);
            }

            state?
                .map(ShardAccountFromCache::Found)
                .unwrap_or(ShardAccountFromCache::NotFound)
        })
    }
}

pub enum ShardAccountFromCache {
    NotReady,
    NotFound,
    Found(ShardAccount),
}

pub struct ShardAccount {
    pub data: ton_types::Cell,
    pub last_transaction_id: nekoton_abi::LastTransactionId,
    pub state_handle: Arc<RefMcStateHandle>,
    pub gen_utime: u32,
}

struct ShardAccounts {
    accounts: ton_block::ShardAccounts,
    state_handle: Arc<RefMcStateHandle>,
    gen_utime: u32,
}

impl ShardAccounts {
    fn get(&self, account: &ton_types::UInt256) -> Result<Option<ShardAccount>> {
        match self.accounts.get(account)? {
            Some(account) => Ok(Some(ShardAccount {
                data: account.account_cell(),
                last_transaction_id: nekoton_abi::LastTransactionId::Exact(
                    nekoton_abi::TransactionId {
                        lt: account.last_trans_lt(),
                        hash: *account.last_trans_hash(),
                    },
                ),
                state_handle: self.state_handle.clone(),
                gen_utime: self.gen_utime,
            })),
            None => Ok(None),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DbOptions {
    pub max_memory_usage: usize,
    pub min_caches_capacity: usize,
    pub min_compaction_memory_budget: usize,
}

impl Default for DbOptions {
    fn default() -> Self {
        Self {
            max_memory_usage: 2 << 30,             // 2 GB
            min_caches_capacity: 64 << 20,         // 64 MB
            min_compaction_memory_budget: 1 << 30, // 1 GB
        }
    }
}

pub struct PersistentStorage {
    pub transactions: Table<tables::Transactions>,
    pub transactions_by_hash: Table<tables::TransactionsByHash>,
    pub transactions_by_in_msg: Table<tables::TransactionsByInMsg>,
    pub code_hashes: Table<tables::CodeHashes>,
    pub code_hashes_by_address: Table<tables::CodeHashesByAddress>,

    pub snapshot: ArcSwapOption<OwnedSnapshot>,
    pub inner: WeeDb,
}

impl PersistentStorage {
    const DB_VERSION: Semver = [0, 1, 0];

    pub fn new(path: &Path, options: &DbOptions) -> Result<Self> {
        let limit = match fdlimit::raise_fd_limit() {
            // New fd limit
            Some(limit) => limit,
            // Current soft limit
            None => {
                rlimit::getrlimit(rlimit::Resource::NOFILE)
                    .unwrap_or((256, 0))
                    .0
            }
        };

        let caches_capacity =
            std::cmp::max(options.max_memory_usage / 3, options.min_caches_capacity);
        let compaction_memory_budget = std::cmp::max(
            options.max_memory_usage - options.max_memory_usage / 3,
            options.min_compaction_memory_budget,
        );

        let caches = Caches::with_capacity(caches_capacity);

        let inner = WeeDb::builder(path, caches)
            .options(|opts, _| {
                opts.set_level_compaction_dynamic_level_bytes(true);

                // compression opts
                opts.set_zstd_max_train_bytes(32 * 1024 * 1024);
                opts.set_compression_type(rocksdb::DBCompressionType::Zstd);

                // io
                opts.set_max_open_files(limit as i32);

                // logging
                opts.set_log_level(rocksdb::LogLevel::Error);
                opts.set_keep_log_file_num(2);
                opts.set_recycle_log_file_num(2);

                // cf
                opts.create_if_missing(true);
                opts.create_missing_column_families(true);

                // cpu
                opts.set_max_background_jobs(std::cmp::max((num_cpus::get() as i32) / 2, 2));
                opts.increase_parallelism(num_cpus::get() as i32);

                opts.optimize_level_style_compaction(compaction_memory_budget);

                // debug
                // opts.enable_statistics();
                // opts.set_stats_dump_period_sec(30);
            })
            .with_table::<tables::Transactions>()
            .with_table::<tables::TransactionsByHash>()
            .with_table::<tables::TransactionsByInMsg>()
            .with_table::<tables::CodeHashes>()
            .with_table::<tables::CodeHashesByAddress>()
            .build()
            .context("Failed building db")?;

        let migrations = Migrations::with_target_version(Self::DB_VERSION);
        inner
            .apply(migrations)
            .context("Failed to apply migrations")?;

        Ok(Self {
            transactions: inner.instantiate_table(),
            transactions_by_hash: inner.instantiate_table(),
            transactions_by_in_msg: inner.instantiate_table(),
            code_hashes: inner.instantiate_table(),
            code_hashes_by_address: inner.instantiate_table(),
            snapshot: Default::default(),
            inner,
        })
    }

    pub fn load_snapshot(&self) -> Option<Arc<OwnedSnapshot>> {
        self.snapshot.load_full()
    }

    pub fn update_snapshot(&self) {
        let snapshot = Arc::new(OwnedSnapshot::new(self.inner.raw().clone()));
        self.snapshot.store(Some(snapshot));
    }

    pub fn update(&self, block_id: &ton_block::BlockIdExt, block: &ton_block::Block) -> Result<()> {
        let workchain = block_id.shard().workchain_id();
        let Ok(workchain) = i8::try_from(workchain) else {
            return Ok(());
        };

        let extra = block.read_extra()?;
        let account_blocks = extra.read_account_blocks()?;

        // Prepare column families
        let mut write_batch = rocksdb::WriteBatch::default();
        let tx_cf = &self.transactions.cf();
        let tx_by_hash_cf = &self.transactions_by_hash.cf();
        let tx_by_in_msg_cf = &self.transactions_by_in_msg.cf();

        // Prepare buffer for full tx id
        let mut tx_full_id = [0u8; { tables::Transactions::KEY_LEN }];
        tx_full_id[0] = workchain as u8;

        // Iterate all changed accounts in block
        let mut non_empty_batch = false;
        account_blocks.iterate_with_keys(|account, value| {
            non_empty_batch |= true;

            // Fill account address in full transaction buffer
            tx_full_id[1..33].copy_from_slice(account.as_slice());

            // Flag to update code hash
            let mut has_special_actions = false;

            // Process account transactions
            value.transactions().iterate_slices(|_, mut value| {
                let tx_cell = value.checked_drain_reference()?;
                let tx_hash = tx_cell.repr_hash();
                let tx_data = ton_types::serialize_toc(&tx_cell)?;
                let tx = ton_block::Transaction::construct_from_cell(tx_cell)?;

                tx_full_id[33..].copy_from_slice(&tx.lt.to_be_bytes());

                // Update code hash
                let descr = tx.read_description()?;
                if let Some(action_phase) = descr.action_phase_ref() {
                    has_special_actions |= action_phase.spec_actions != 0;
                }

                // Write tx data and indices
                write_batch.put_cf(tx_cf, tx_full_id.as_slice(), tx_data);
                write_batch.put_cf(tx_by_hash_cf, tx_hash.as_slice(), tx_full_id.as_slice());
                if let Some(in_msg_cell) = tx.in_msg_cell() {
                    write_batch.put_cf(
                        tx_by_in_msg_cf,
                        in_msg_cell.repr_hash().as_slice(),
                        tx_full_id.as_slice(),
                    );
                }

                Ok(true)
            })?;

            // TODO: Update account code hash if `has_special_actions`

            Ok(true)
        })?;

        if non_empty_batch {
            self.inner
                .raw()
                .write_opt(write_batch, self.transactions.write_config())
                .context("Failed to update JRPC storage")?;
        }

        Ok(())
    }
}

impl Drop for PersistentStorage {
    fn drop(&mut self) {
        self.snapshot.store(None);
        self.inner.raw().cancel_all_background_work(true);
    }
}

pub struct OwnedSnapshot {
    inner: rocksdb::Snapshot<'static>,
    _db: Arc<rocksdb::DB>,
}

impl OwnedSnapshot {
    fn new(db: Arc<rocksdb::DB>) -> Self {
        use rocksdb::Snapshot;

        unsafe fn extend_lifetime<'a>(r: Snapshot<'a>) -> Snapshot<'static> {
            std::mem::transmute::<Snapshot<'a>, Snapshot<'static>>(r)
        }

        // SAFETY: `Snapshot` requires the same lifetime as `rocksdb::DB` but
        // `tokio::task::spawn` requires 'static. This object ensures
        // that `rocksdb::DB` object lifetime will exceed the lifetime of the snapshot
        let inner = unsafe { extend_lifetime(db.as_ref().snapshot()) };
        Self { inner, _db: db }
    }
}

impl std::ops::Deref for OwnedSnapshot {
    type Target = rocksdb::Snapshot<'static>;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

fn contains_account(shard: &ton_block::ShardIdent, account: &ton_types::UInt256) -> bool {
    let shard_prefix = shard.shard_prefix_with_tag();
    if shard_prefix == ton_block::SHARD_FULL {
        true
    } else {
        let len = shard.prefix_len();
        let account_prefix = account_prefix(account, len as usize) >> (64 - len);
        let shard_prefix = shard_prefix >> (64 - len);
        account_prefix == shard_prefix
    }
}

fn account_prefix(account: &ton_types::UInt256, len: usize) -> u64 {
    debug_assert!(len <= 64);

    let account = account.as_slice();

    let mut value: u64 = 0;

    let bytes = len / 8;
    for (i, byte) in account.iter().enumerate().take(bytes) {
        value |= (*byte as u64) << (8 * (7 - i));
    }

    let remainder = len % 8;
    if remainder > 0 {
        let r = account[bytes] >> (8 - remainder);
        value |= (r as u64) << (8 * (7 - bytes) + 8 - remainder);
    }

    value
}
