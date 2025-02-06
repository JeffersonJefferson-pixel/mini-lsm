#![allow(dead_code)] // REMOVE THIS LINE after fully implementing this functionality

use std::collections::HashMap;
use std::fs::{self, File};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use anyhow::Result;
use bytes::Bytes;
use parking_lot::{Mutex, MutexGuard, RwLock};

use crate::block::Block;
use crate::compact::{
    CompactionController, CompactionOptions, LeveledCompactionController, LeveledCompactionOptions,
    SimpleLeveledCompactionController, SimpleLeveledCompactionOptions, TieredCompactionController,
};
use crate::iterators::concat_iterator::SstConcatIterator;
use crate::iterators::merge_iterator::MergeIterator;
use crate::iterators::two_merge_iterator::TwoMergeIterator;
use crate::iterators::StorageIterator;
use crate::key::{KeyBytes, KeySlice};
use crate::lsm_iterator::{FusedIterator, LsmIterator};
use crate::manifest::Manifest;
use crate::mem_table::{map_bound, MemTable};
use crate::mvcc::LsmMvccInner;
use crate::table::{FileObject, SsTable, SsTableBuilder, SsTableIterator};

pub type BlockCache = moka::sync::Cache<(usize, usize), Arc<Block>>;

/// Represents the state of the storage engine.
#[derive(Clone)]
pub struct LsmStorageState {
    /// The current memtable.
    pub memtable: Arc<MemTable>,
    /// Immutable memtables, from latest to earliest.
    pub imm_memtables: Vec<Arc<MemTable>>,
    /// L0 SSTs, from latest to earliest.
    pub l0_sstables: Vec<usize>,
    /// SsTables sorted by key range; L1 - L_max for leveled compaction, or tiers for tiered
    /// compaction.
    pub levels: Vec<(usize, Vec<usize>)>,
    /// SST objects.
    pub sstables: HashMap<usize, Arc<SsTable>>,
}

pub enum WriteBatchRecord<T: AsRef<[u8]>> {
    Put(T, T),
    Del(T),
}

impl LsmStorageState {
    fn create(options: &LsmStorageOptions) -> Self {
        let levels = match &options.compaction_options {
            CompactionOptions::Leveled(LeveledCompactionOptions { max_levels, .. })
            | CompactionOptions::Simple(SimpleLeveledCompactionOptions { max_levels, .. }) => (1
                ..=*max_levels)
                .map(|level| (level, Vec::new()))
                .collect::<Vec<_>>(),
            CompactionOptions::Tiered(_) => Vec::new(),
            CompactionOptions::NoCompaction => vec![(1, Vec::new())],
        };
        Self {
            memtable: Arc::new(MemTable::create(0)),
            imm_memtables: Vec::new(),
            l0_sstables: Vec::new(),
            levels,
            sstables: Default::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LsmStorageOptions {
    // Block size in bytes
    pub block_size: usize,
    // SST size in bytes, also the approximate memtable capacity limit
    pub target_sst_size: usize,
    // Maximum number of memtables in memory, flush to L0 when exceeding this limit
    pub num_memtable_limit: usize,
    pub compaction_options: CompactionOptions,
    pub enable_wal: bool,
    pub serializable: bool,
}

impl LsmStorageOptions {
    pub fn default_for_week1_test() -> Self {
        Self {
            block_size: 4096,
            target_sst_size: 2 << 20,
            compaction_options: CompactionOptions::NoCompaction,
            enable_wal: false,
            num_memtable_limit: 50,
            serializable: false,
        }
    }

    pub fn default_for_week1_day6_test() -> Self {
        Self {
            block_size: 4096,
            target_sst_size: 2 << 20,
            compaction_options: CompactionOptions::NoCompaction,
            enable_wal: false,
            num_memtable_limit: 2,
            serializable: false,
        }
    }

    pub fn default_for_week2_test(compaction_options: CompactionOptions) -> Self {
        Self {
            block_size: 4096,
            target_sst_size: 1 << 20, // 1MB
            compaction_options,
            enable_wal: false,
            num_memtable_limit: 2,
            serializable: false,
        }
    }
}

#[derive(Clone, Debug)]
pub enum CompactionFilter {
    Prefix(Bytes),
}

/// The storage interface of the LSM tree.
pub(crate) struct LsmStorageInner {
    pub(crate) state: Arc<RwLock<Arc<LsmStorageState>>>,
    pub(crate) state_lock: Mutex<()>,
    path: PathBuf,
    pub(crate) block_cache: Arc<BlockCache>,
    next_sst_id: AtomicUsize,
    pub(crate) options: Arc<LsmStorageOptions>,
    pub(crate) compaction_controller: CompactionController,
    pub(crate) manifest: Option<Manifest>,
    pub(crate) mvcc: Option<LsmMvccInner>,
    pub(crate) compaction_filters: Arc<Mutex<Vec<CompactionFilter>>>,
}

/// A thin wrapper for `LsmStorageInner` and the user interface for MiniLSM.
pub struct MiniLsm {
    pub(crate) inner: Arc<LsmStorageInner>,
    /// Notifies the L0 flush thread to stop working. (In week 1 day 6)
    flush_notifier: crossbeam_channel::Sender<()>,
    /// The handle for the flush thread. (In week 1 day 6)
    flush_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Notifies the compaction thread to stop working. (In week 2)
    compaction_notifier: crossbeam_channel::Sender<()>,
    /// The handle for the compaction thread. (In week 2)
    compaction_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Drop for MiniLsm {
    fn drop(&mut self) {
        self.compaction_notifier.send(()).ok();
        self.flush_notifier.send(()).ok();
    }
}

impl MiniLsm {
    pub fn close(&self) -> Result<()> {
        self.inner.sync_dir()?;
        self.flush_notifier.send(()).ok();
        // wait for flush thread to finish.
        let mut flush_thread = self.flush_thread.lock();
        if let Some(flush_thread) = flush_thread.take() {
            flush_thread
                .join()
                .map_err(|e| anyhow::anyhow!("{:?}", e))?;
        }
        // freeze and flush remaining memtables
        if !self.inner.state.read().memtable.is_empty() {
            self.inner
                .force_freeze_memtable(&self.inner.state_lock.lock())?;
        }
        while {
            let snapshot = self.inner.state.read();
            !snapshot.imm_memtables.is_empty()
        } {
            self.inner.force_flush_next_imm_memtable()?;
        }

        self.inner.sync_dir()?;

        Ok(())
    }

    /// Start the storage engine by either loading an existing directory or creating a new one if the directory does
    /// not exist.
    pub fn open(path: impl AsRef<Path>, options: LsmStorageOptions) -> Result<Arc<Self>> {
        let inner = Arc::new(LsmStorageInner::open(path, options)?);
        let (tx1, rx) = crossbeam_channel::unbounded();
        let compaction_thread = inner.spawn_compaction_thread(rx)?;
        let (tx2, rx) = crossbeam_channel::unbounded();
        let flush_thread = inner.spawn_flush_thread(rx)?;
        Ok(Arc::new(Self {
            inner,
            flush_notifier: tx2,
            flush_thread: Mutex::new(flush_thread),
            compaction_notifier: tx1,
            compaction_thread: Mutex::new(compaction_thread),
        }))
    }

    pub fn new_txn(&self) -> Result<()> {
        self.inner.new_txn()
    }

    pub fn write_batch<T: AsRef<[u8]>>(&self, batch: &[WriteBatchRecord<T>]) -> Result<()> {
        self.inner.write_batch(batch)
    }

    pub fn add_compaction_filter(&self, compaction_filter: CompactionFilter) {
        self.inner.add_compaction_filter(compaction_filter)
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Bytes>> {
        self.inner.get(key)
    }

    pub fn put(&self, key: &[u8], value: &[u8]) -> Result<()> {
        self.inner.put(key, value)
    }

    pub fn delete(&self, key: &[u8]) -> Result<()> {
        self.inner.delete(key)
    }

    pub fn sync(&self) -> Result<()> {
        self.inner.sync()
    }

    pub fn scan(
        &self,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
    ) -> Result<FusedIterator<LsmIterator>> {
        self.inner.scan(lower, upper)
    }

    /// Only call this in test cases due to race conditions
    pub fn force_flush(&self) -> Result<()> {
        if !self.inner.state.read().memtable.is_empty() {
            self.inner
                .force_freeze_memtable(&self.inner.state_lock.lock())?;
        }
        if !self.inner.state.read().imm_memtables.is_empty() {
            self.inner.force_flush_next_imm_memtable()?;
        }
        Ok(())
    }

    pub fn force_full_compaction(&self) -> Result<()> {
        self.inner.force_full_compaction()
    }
}

fn key_within(user_key: &[u8], table_begin: &[u8], table_end: &[u8]) -> bool {
    table_begin <= user_key && user_key <= table_end
}

fn range_overlap(
    user_begin: Bound<&[u8]>,
    user_end: Bound<&[u8]>,
    table_begin: &[u8],
    table_end: &[u8],
) -> bool {
    match user_end {
        Bound::Excluded(key) => {
            return !(key <= table_begin);
        }
        Bound::Included(key) => return !(key < table_begin),
        _ => {}
    }
    match user_begin {
        Bound::Excluded(key) => {
            return !(key >= table_end);
        }
        Bound::Included(key) => {
            return !(key > table_end);
        }
        _ => {}
    }
    true
}

impl LsmStorageInner {
    pub(crate) fn next_sst_id(&self) -> usize {
        self.next_sst_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    }

    /// Start the storage engine by either loading an existing directory or creating a new one if the directory does
    /// not exist.
    pub(crate) fn open(path: impl AsRef<Path>, options: LsmStorageOptions) -> Result<Self> {
        let path = path.as_ref();

        // create lsm directory if it does not exist
        if !path.exists() {
            fs::create_dir_all(path)?;
        }

        let state = LsmStorageState::create(&options);

        let compaction_controller = match &options.compaction_options {
            CompactionOptions::Leveled(options) => {
                CompactionController::Leveled(LeveledCompactionController::new(options.clone()))
            }
            CompactionOptions::Tiered(options) => {
                CompactionController::Tiered(TieredCompactionController::new(options.clone()))
            }
            CompactionOptions::Simple(options) => CompactionController::Simple(
                SimpleLeveledCompactionController::new(options.clone()),
            ),
            CompactionOptions::NoCompaction => CompactionController::NoCompaction,
        };

        let storage = Self {
            state: Arc::new(RwLock::new(Arc::new(state))),
            state_lock: Mutex::new(()),
            path: path.to_path_buf(),
            block_cache: Arc::new(BlockCache::new(1024)),
            next_sst_id: AtomicUsize::new(1),
            compaction_controller,
            manifest: None,
            options: options.into(),
            mvcc: None,
            compaction_filters: Arc::new(Mutex::new(Vec::new())),
        };

        Ok(storage)
    }

    pub fn sync(&self) -> Result<()> {
        unimplemented!()
    }

    pub fn add_compaction_filter(&self, compaction_filter: CompactionFilter) {
        let mut compaction_filters = self.compaction_filters.lock();
        compaction_filters.push(compaction_filter);
    }

    /// Get a key from the storage. In day 7, this can be further optimized by using a bloom filter.
    pub fn get(&self, _key: &[u8]) -> Result<Option<Bytes>> {
        // get read lock.
        let guard = self.state.read();
        let snapshot = &guard;

        // read from current memtable.
        if let Some(value) = snapshot.memtable.get(_key) {
            if value.is_empty() {
                // tombstone.
                return Ok(None);
            }
            return Ok(Some(value));
        }

        // read from earlier immutable memtables.
        for memtable in snapshot.imm_memtables.iter() {
            if let Some(value) = memtable.get(_key) {
                if value.is_empty() {
                    // tombstone.
                    return Ok(None);
                }
                return Ok(Some(value));
            }
        }

        // scans on ssts.
        let mut iters = Vec::with_capacity(snapshot.l0_sstables.len());
        for sst_id in snapshot.l0_sstables.iter() {
            let sst = snapshot.sstables[sst_id].clone();
            if key_within(_key, sst.first_key().raw_ref(), sst.last_key().raw_ref()) {
                // check bloom filter
                if let Some(bloom) = &sst.bloom {
                    // key hash
                    let key_hash: u32 = farmhash::fingerprint32(_key);
                    if bloom.may_contain(key_hash) {
                        iters.push(Box::new(SsTableIterator::create_and_seek_to_key(
                            sst,
                            KeySlice::from_slice(_key),
                        )?));
                    }
                } else {
                    iters.push(Box::new(SsTableIterator::create_and_seek_to_key(
                        sst,
                        KeySlice::from_slice(_key),
                    )?));
                }
            }
        }
        // create merge iterator
        let l0_iter = MergeIterator::create(iters);
        
        // l1 ssts
        let mut l1_ssts = Vec::with_capacity(snapshot.levels[0].1.len());
        for sst_id in snapshot.levels[0].1.iter() {
            let sst = snapshot.sstables[sst_id].clone();
            l1_ssts.push(sst);
        }
        // l1 concat iterators
        let l1_iter = SstConcatIterator::create_and_seek_to_key(l1_ssts, KeySlice::from_slice(_key))?;

        // two merge iterator of l0 and l1 ssts
        let iter = TwoMergeIterator::create(l0_iter, l1_iter)?;

        if iter.is_valid()
            && iter.key() == KeySlice::from_slice(_key)
            && !iter.value().is_empty()
        {
            return Ok(Some(Bytes::copy_from_slice(iter.value())));
        }

        Ok(None)
    }

    /// Write a batch of data into the storage. Implement in week 2 day 7.
    pub fn write_batch<T: AsRef<[u8]>>(&self, _batch: &[WriteBatchRecord<T>]) -> Result<()> {
        unimplemented!()
    }

    /// Put a key-value pair into the storage by writing into the current memtable.
    pub fn put(&self, _key: &[u8], _value: &[u8]) -> Result<()> {
        // validate key and value
        assert!(!_key.is_empty(), "key cannot be empty");
        assert!(!_value.is_empty(), "value cannot be empty");

        let size;
        {
            // get read lock on state.
            let guard = self.state.read();
            // put key/value in memtable
            guard.memtable.put(_key, _value)?;
            // check memtable size.
            size = guard.memtable.approximate_size();
        }

        self.try_freeze(size)?;

        Ok(())
    }

    /// Remove a key from the storage by writing an empty value.
    pub fn delete(&self, _key: &[u8]) -> Result<()> {
        // validate key
        assert!(!_key.is_empty(), "key cannot be empty");

        let size;
        {
            // get read lock on state.
            let guard = self.state.read();
            // put an empty slice for the key in memtable.
            guard.memtable.put(_key, b"")?;
            // check memtable size.
            size = guard.memtable.approximate_size();
        }

        self.try_freeze(size)?;

        Ok(())
    }

    fn try_freeze(&self, estimated_size: usize) -> Result<()> {
        if estimated_size >= self.options.target_sst_size {
            // synchronize through state lock.
            let state_lock = self.state_lock.lock();
            let guard = self.state.read();
            // check size again.
            if guard.memtable.approximate_size() >= self.options.target_sst_size {
                drop(guard);
                self.force_freeze_memtable(&state_lock)?;
            }
        }
        Ok(())
    }

    pub(crate) fn path_of_sst_static(path: impl AsRef<Path>, id: usize) -> PathBuf {
        path.as_ref().join(format!("{:05}.sst", id))
    }

    pub(crate) fn path_of_sst(&self, id: usize) -> PathBuf {
        Self::path_of_sst_static(&self.path, id)
    }

    pub(crate) fn path_of_wal_static(path: impl AsRef<Path>, id: usize) -> PathBuf {
        path.as_ref().join(format!("{:05}.wal", id))
    }

    pub(crate) fn path_of_wal(&self, id: usize) -> PathBuf {
        Self::path_of_wal_static(&self.path, id)
    }

    pub(super) fn sync_dir(&self) -> Result<()> {
        File::open(&self.path)?.sync_all()?;
        Ok(())
    }

    /// Force freeze the current memtable to an immutable memtable
    pub fn force_freeze_memtable(&self, _state_lock_observer: &MutexGuard<'_, ()>) -> Result<()> {
        // create new memtable.
        let memtable_id: usize = self.next_sst_id();
        let memtable = Arc::new(MemTable::create(memtable_id));

        let old_memtable;
        {
            let mut guard = self.state.write();
            let mut snapshot = guard.as_ref().clone();
            // swap current memtable with new one.
            old_memtable = std::mem::replace(&mut snapshot.memtable, memtable);
            // add old memtable to immutable.
            snapshot.imm_memtables.insert(0, old_memtable.clone());
            // update the snapshot.
            *guard = Arc::new(snapshot);
        }

        Ok(())
    }

    /// Force flush the earliest-created immutable memtable to disk
    pub fn force_flush_next_imm_memtable(&self) -> Result<()> {
        let _state_lock = self.state_lock.lock();

        // select memtable to flush
        let memtable = {
            let guard = self.state.read();
            guard.imm_memtables.last().unwrap().clone()
        };

        // create sst
        let mut builder = SsTableBuilder::new(self.options.block_size);
        memtable.flush(&mut builder)?;
        let id = memtable.id();
        let sst =
            Arc::new(builder.build(id, Some(self.block_cache.clone()), self.path_of_sst(id))?);

        {
            let mut guard = self.state.write();
            let mut snapshot = guard.as_ref().clone();
            // remove memtable from immutable memtable list
            snapshot.imm_memtables.pop().unwrap();
            // add sst to l0 ssts.
            snapshot.l0_sstables.insert(0, id);
            snapshot.sstables.insert(id, sst);
            *guard = Arc::new(snapshot);
        }

        self.sync_dir()?;

        Ok(())
    }

    pub fn new_txn(&self) -> Result<()> {
        // no-op
        Ok(())
    }

    /// Create an iterator over a range of keys.
    pub fn scan(
        &self,
        _lower: Bound<&[u8]>,
        _upper: Bound<&[u8]>,
    ) -> Result<FusedIterator<LsmIterator>> {
        // clone state
        let snapshot = {
            let guard = self.state.read();
            Arc::clone(&guard)
        };

        // memtable
        let mut memtable_iters = Vec::with_capacity(snapshot.imm_memtables.len() + 1);
        memtable_iters.push(Box::new(snapshot.memtable.scan(_lower, _upper)));
        snapshot
            .imm_memtables
            .iter()
            .for_each(|memtable: &Arc<MemTable>| {
                memtable_iters.push(Box::new(memtable.scan(_lower, _upper)))
            });

        let memtable_merge_iter = MergeIterator::create(memtable_iters);

        // sst
        let mut sst_iters = Vec::with_capacity(snapshot.l0_sstables.len());
        for sst_id in snapshot.l0_sstables.iter() {
            let sst = snapshot.sstables[sst_id].clone();
            if range_overlap(
                _lower,
                _upper,
                sst.first_key().raw_ref(),
                sst.last_key().raw_ref(),
            ) {
                let sst_iter = match _lower {
                    // bound
                    Bound::Included(key) => {
                        SsTableIterator::create_and_seek_to_key(sst, KeySlice::from_slice(key))
                            .unwrap()
                    }
                    Bound::Excluded(key) => {
                        let mut iter =
                            SsTableIterator::create_and_seek_to_key(sst, KeySlice::from_slice(key))
                                .unwrap();
                        if iter.is_valid() && iter.key() == KeySlice::from_slice(key) {
                            iter.next().unwrap();
                        }
                        iter
                    }
                    Bound::Unbounded => SsTableIterator::create_and_seek_to_first(sst).unwrap(),
                };
                sst_iters.push(Box::new(sst_iter))
            }
        }
        let sst_merge_iter = MergeIterator::create(sst_iters);

        // two merge iter
        let l0_iter = TwoMergeIterator::create(memtable_merge_iter, sst_merge_iter).unwrap();
        
        // l1 ssts
        let mut l1_ssts = Vec::with_capacity(snapshot.levels[0].1.len());
        for sst_id in snapshot.levels[0].1.iter() {
            let sst = snapshot.sstables[sst_id].clone();
            l1_ssts.push(sst);
        }

        // l1 sst concat iter
        let l1_iter = match _lower {
            Bound::Included(key) => SstConcatIterator::create_and_seek_to_key(l1_ssts, KeySlice::from_slice(key))?,
            Bound::Excluded(key) => {
                let mut iter = SstConcatIterator::create_and_seek_to_key(l1_ssts, KeySlice::from_slice(key))?;
                if iter.is_valid() && iter.key().into_inner() == key {
                    iter.next()?;
                }
                iter
            }
            Bound::Unbounded => SstConcatIterator::create_and_seek_to_first(l1_ssts)?
        };

        // two merge iterator with l1 concat iterator
        let iter = TwoMergeIterator::create(l0_iter, l1_iter)?;
        
        let lsm_iterator = LsmIterator::new(iter, map_bound(_upper))?;

        Ok(FusedIterator::new(lsm_iterator))
    }
}
