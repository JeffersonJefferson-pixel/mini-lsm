#![allow(dead_code)] // REMOVE THIS LINE after fully implementing this functionality

mod leveled;
mod simple_leveled;
mod tiered;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
pub use leveled::{LeveledCompactionController, LeveledCompactionOptions, LeveledCompactionTask};
use serde::{Deserialize, Serialize};
pub use simple_leveled::{
    SimpleLeveledCompactionController, SimpleLeveledCompactionOptions, SimpleLeveledCompactionTask,
};
pub use tiered::{TieredCompactionController, TieredCompactionOptions, TieredCompactionTask};

use crate::iterators::concat_iterator::SstConcatIterator;
use crate::iterators::merge_iterator::MergeIterator;
use crate::iterators::two_merge_iterator::TwoMergeIterator;
use crate::iterators::StorageIterator;
use crate::lsm_storage::{LsmStorageInner, LsmStorageState};
use crate::table::{SsTable, SsTableBuilder, SsTableIterator};

#[derive(Debug, Serialize, Deserialize)]
pub enum CompactionTask {
    Leveled(LeveledCompactionTask),
    Tiered(TieredCompactionTask),
    Simple(SimpleLeveledCompactionTask),
    ForceFullCompaction {
        l0_sstables: Vec<usize>,
        l1_sstables: Vec<usize>,
    },
}

impl CompactionTask {
    fn compact_to_bottom_level(&self) -> bool {
        match self {
            CompactionTask::ForceFullCompaction { .. } => true,
            CompactionTask::Leveled(task) => task.is_lower_level_bottom_level,
            CompactionTask::Simple(task) => task.is_lower_level_bottom_level,
            CompactionTask::Tiered(task) => task.bottom_tier_included,
        }
    }
}

pub(crate) enum CompactionController {
    Leveled(LeveledCompactionController),
    Tiered(TieredCompactionController),
    Simple(SimpleLeveledCompactionController),
    NoCompaction,
}

impl CompactionController {
    pub fn generate_compaction_task(&self, snapshot: &LsmStorageState) -> Option<CompactionTask> {
        match self {
            CompactionController::Leveled(ctrl) => ctrl
                .generate_compaction_task(snapshot)
                .map(CompactionTask::Leveled),
            CompactionController::Simple(ctrl) => ctrl
                .generate_compaction_task(snapshot)
                .map(CompactionTask::Simple),
            CompactionController::Tiered(ctrl) => ctrl
                .generate_compaction_task(snapshot)
                .map(CompactionTask::Tiered),
            CompactionController::NoCompaction => unreachable!(),
        }
    }

    pub fn apply_compaction_result(
        &self,
        snapshot: &LsmStorageState,
        task: &CompactionTask,
        output: &[usize],
        in_recovery: bool,
    ) -> (LsmStorageState, Vec<usize>) {
        match (self, task) {
            (CompactionController::Leveled(ctrl), CompactionTask::Leveled(task)) => {
                ctrl.apply_compaction_result(snapshot, task, output, in_recovery)
            }
            (CompactionController::Simple(ctrl), CompactionTask::Simple(task)) => {
                ctrl.apply_compaction_result(snapshot, task, output)
            }
            (CompactionController::Tiered(ctrl), CompactionTask::Tiered(task)) => {
                ctrl.apply_compaction_result(snapshot, task, output)
            }
            _ => unreachable!(),
        }
    }
}

impl CompactionController {
    pub fn flush_to_l0(&self) -> bool {
        matches!(
            self,
            Self::Leveled(_) | Self::Simple(_) | Self::NoCompaction
        )
    }
}

#[derive(Debug, Clone)]
pub enum CompactionOptions {
    /// Leveled compaction with partial compaction + dynamic level support (= RocksDB's Leveled
    /// Compaction)
    Leveled(LeveledCompactionOptions),
    /// Tiered compaction (= RocksDB's universal compaction)
    Tiered(TieredCompactionOptions),
    /// Simple leveled compaction
    Simple(SimpleLeveledCompactionOptions),
    /// In no compaction mode (week 1), always flush to L0
    NoCompaction,
}

impl LsmStorageInner {
    fn compact(&self, _task: &CompactionTask) -> Result<Vec<Arc<SsTable>>> {
        let mut new_ssts = Vec::new();
        let snapshot = {
            let state = self.state.read();
            state.clone()
        };

        // create sst iterators for l0 and l1 ssts.
        let mut iter = match _task {
            CompactionTask::ForceFullCompaction {
                l0_sstables,
                l1_sstables,
            } => {
                // l0 iter
                let mut l0_iters = Vec::with_capacity(l0_sstables.len());  
                for sst_id in l0_sstables.iter() {
                    let sst = snapshot.sstables[sst_id].clone();
                    l0_iters.push(Box::new(SsTableIterator::create_and_seek_to_first(sst)?));
                }
                let l0_iter = MergeIterator::create(l0_iters);
                
                // l1 iter
                let mut l1_ssts = Vec::with_capacity(l1_sstables.len());
                for sst_id in l1_sstables.iter() {
                    let sst = snapshot.sstables[sst_id].clone();
                    l1_ssts.push(sst);
                }
                let l1_iter = SstConcatIterator::create_and_seek_to_first(l1_ssts)?;

                TwoMergeIterator::create(l0_iter, l1_iter)?
                
            }
            _ => unimplemented!(),
        };

        let mut builder = None;
        
        let compact_to_bottom_level = _task.compact_to_bottom_level();

        while iter.is_valid() {
            // create new sst builder.
            if builder.is_none() {
                builder = Some(SsTableBuilder::new(self.options.block_size));
            }
            let builder_inner = builder.as_mut().unwrap();
            if compact_to_bottom_level {
                if !iter.value().is_empty() {
                    // handle delete marker
                    builder_inner.add(iter.key(), iter.value());
                }
            } else {
                builder_inner.add(iter.key(), iter.value());
            }

            iter.next()?;

            // handle full sst.
            if builder_inner.estimated_size() >= self.options.target_sst_size {
                let sst_id = self.next_sst_id();
                // builder is reset.
                let builder = builder.take().unwrap();
                let sst = builder.build(
                    sst_id,
                    Some(self.block_cache.clone()),
                    self.path_of_sst(sst_id),
                )?;
                new_ssts.push(Arc::new(sst));
            }
        }

        // handle case last sst is not yet full but iterator reaches the end.
        if let Some(builder) = builder {
            let sst_id = self.next_sst_id();
            let sst = builder.build(
                sst_id,
                Some(self.block_cache.clone()),
                self.path_of_sst(sst_id),
            )?;
            new_ssts.push(Arc::new(sst));
        }

        Ok(new_ssts)
    }

    pub fn force_full_compaction(&self) -> Result<()> {
        // choose l0 and l1 ssts to compact.
        let snapshot = {
            let state = self.state.read();
            state.clone()
        };
        let l0 = snapshot.l0_sstables.clone();
        let l1 = snapshot.levels[0].1.clone();
        let task = CompactionTask::ForceFullCompaction {
            l0_sstables: l0.clone(),
            l1_sstables: l1.clone(),
        };
        let new_sst = self.compact(&task)?;

        // take state lock at the end of compaction to update lsm state.
        {
            let _state_lock = self.state_lock.lock();
            let mut state = self.state.read().as_ref().clone();
            // remove old l0 from l0.
            state.l0_sstables = state
                .l0_sstables
                .iter()
                .filter(|x| !l0.contains(x))
                .copied()
                .collect::<Vec<_>>();
            // update l1 to new compacted ssts.
            state.levels[0].1 = new_sst
                .iter()
                .map(|sst| sst.sst_id())
                .collect::<Vec<_>>();
            // remove old l0 and old l1 from sstables
            for sst in l0.iter().chain(l1.iter()) {
                state.sstables.remove(sst);
            }
            // add new ssts to sstables
            for sst in new_sst {
                state.sstables.insert(sst.sst_id(), sst);
            }

            // update lsm state.
            *self.state.write() = Arc::new(state);
        }

        // remove l0 and l1 sst files.
        for sst in l0.iter().chain(l1.iter()) {
            std::fs::remove_file(self.path_of_sst(*sst))?;
        }
        Ok(())
    }

    fn trigger_compaction(&self) -> Result<()> {
        unimplemented!()
    }

    pub(crate) fn spawn_compaction_thread(
        self: &Arc<Self>,
        rx: crossbeam_channel::Receiver<()>,
    ) -> Result<Option<std::thread::JoinHandle<()>>> {
        if let CompactionOptions::Leveled(_)
        | CompactionOptions::Simple(_)
        | CompactionOptions::Tiered(_) = self.options.compaction_options
        {
            let this = self.clone();
            let handle = std::thread::spawn(move || {
                let ticker = crossbeam_channel::tick(Duration::from_millis(50));
                loop {
                    crossbeam_channel::select! {
                        recv(ticker) -> _ => if let Err(e) = this.trigger_compaction() {
                            eprintln!("compaction failed: {}", e);
                        },
                        recv(rx) -> _ => return
                    }
                }
            });
            return Ok(Some(handle));
        }
        Ok(None)
    }

    fn trigger_flush(&self) -> Result<()> {
        // flush if number of memtables exceeds limit
        if {
            let snapshot = self.state.read();
            snapshot.imm_memtables.len() >= self.options.num_memtable_limit
        } {
            self.force_flush_next_imm_memtable()?;
        }

        Ok(())
    }

    pub(crate) fn spawn_flush_thread(
        self: &Arc<Self>,
        rx: crossbeam_channel::Receiver<()>,
    ) -> Result<Option<std::thread::JoinHandle<()>>> {
        let this = self.clone();
        let handle = std::thread::spawn(move || {
            let ticker = crossbeam_channel::tick(Duration::from_millis(50));
            loop {
                crossbeam_channel::select! {
                    recv(ticker) -> _ => if let Err(e) = this.trigger_flush() {
                        eprintln!("flush failed: {}", e);
                    },
                    recv(rx) -> _ => return
                }
            }
        });
        Ok(Some(handle))
    }
}
