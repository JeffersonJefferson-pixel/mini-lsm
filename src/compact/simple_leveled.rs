use serde::{Deserialize, Serialize};

use crate::lsm_storage::LsmStorageState;

#[derive(Debug, Clone)]
pub struct SimpleLeveledCompactionOptions {
    pub size_ratio_percent: usize,
    pub level0_file_num_compaction_trigger: usize,
    pub max_levels: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SimpleLeveledCompactionTask {
    // if upper_level is `None`, then it is L0 compaction
    pub upper_level: Option<usize>,
    pub upper_level_sst_ids: Vec<usize>,
    pub lower_level: usize,
    pub lower_level_sst_ids: Vec<usize>,
    pub is_lower_level_bottom_level: bool,
}

pub struct SimpleLeveledCompactionController {
    options: SimpleLeveledCompactionOptions,
}

impl SimpleLeveledCompactionController {
    pub fn new(options: SimpleLeveledCompactionOptions) -> Self {
        Self { options }
    }

    /// Generates a compaction task.
    ///
    /// Returns `None` if no compaction needs to be scheduled. The order of SSTs in the compaction task id vector matters.
    pub fn generate_compaction_task(
        &self,
        _snapshot: &LsmStorageState,
    ) -> Option<SimpleLeveledCompactionTask> {
        let mut level_sizes = Vec::new();
        level_sizes.push(_snapshot.l0_sstables.len());
        for (_, level) in &_snapshot.levels {
            level_sizes.push(level.len())
        }

        // iterate over each sst level
        for i in 0..self.options.max_levels {
            let lower_level = i + 1;

            // level 0 trigger    
            if i == 0 && _snapshot.l0_sstables.len() < self.options.level0_file_num_compaction_trigger {
                continue;
            }   
            // size ratio trigger
            let ratio = level_sizes[lower_level] as f64 / level_sizes[i] as f64;
            if ratio * 100.0 < self.options.size_ratio_percent as f64 {
                return Some(SimpleLeveledCompactionTask{
                    upper_level: if i == 0 { None  } else { Some(i) },
                    upper_level_sst_ids: if i == 0 { _snapshot.l0_sstables.clone() } else { _snapshot.levels[i - 1].1.clone() },
                    lower_level: lower_level,
                    lower_level_sst_ids: _snapshot.levels[lower_level - 1].1.clone(),
                    is_lower_level_bottom_level: lower_level == self.options.max_levels,
                });   
            }        
        } 
        

        None
    }

    /// Apply the compaction result.
    ///
    /// The compactor will call this function with the compaction task and the list of SST ids generated. This function applies the
    /// result and generates a new LSM state. The functions should only change `l0_sstables` and `levels` without changing memtables
    /// and `sstables` hash map. Though there should only be one thread running compaction jobs, you should think about the case
    /// where an L0 SST gets flushed while the compactor generates new SSTs, and with that in mind, you should do some sanity checks
    /// in your implementation.
    pub fn apply_compaction_result(
        &self,
        _snapshot: &LsmStorageState,
        _task: &SimpleLeveledCompactionTask,
        _output: &[usize],
    ) -> (LsmStorageState, Vec<usize>) {
        let mut snapshot = _snapshot.clone();
        let mut files_to_remove = Vec::new();
        if let Some(upper_level) = _task.upper_level {
            // case upper level is not level 0
            files_to_remove.extend(&snapshot.levels[upper_level - 1].1);
            snapshot.levels[upper_level - 1].1.clear();
        } else {
            // case upper level is level 0
            files_to_remove.extend(&snapshot.l0_sstables);
            snapshot.l0_sstables.clear();
        }
        files_to_remove.extend(&snapshot.levels[_task.lower_level - 1].1);
        snapshot.levels[_task.lower_level - 1].1 = _output.to_vec();

        (snapshot, files_to_remove) 
    }
}
