#![allow(unused_variables)] // TODO(you): remove this lint after implementing this mod
#![allow(dead_code)] // TODO(you): remove this lint after implementing this mod

use std::sync::Arc;

use anyhow::Result;

use super::StorageIterator;
use crate::{
    key::KeySlice,
    table::{SsTable, SsTableIterator},
};

/// Concat multiple iterators ordered in key order and their key ranges do not overlap. We do not want to create the
/// iterators when initializing this iterator to reduce the overhead of seeking.
pub struct SstConcatIterator {
    current: Option<SsTableIterator>,
    next_sst_idx: usize,
    sstables: Vec<Arc<SsTable>>,
}

impl SstConcatIterator {
    pub fn create_and_seek_to_first(sstables: Vec<Arc<SsTable>>) -> Result<Self> {
        Self::check_sst_valid(&sstables);
        // handle empty ssts
        if sstables.is_empty() {
            return Ok(Self {
                current: None,
                next_sst_idx: 0,
                sstables,
            });
        }
        let mut iter = Self {
            current: Some(SsTableIterator::create_and_seek_to_first(
                sstables[0].clone(),
            )?),
            next_sst_idx: 1,
            sstables,
        };

        iter.move_until_valid()?;

        Ok(iter)
    }

    pub fn create_and_seek_to_key(sstables: Vec<Arc<SsTable>>, key: KeySlice) -> Result<Self> {
        Self::check_sst_valid(&sstables);
        // find key
        let idx = sstables
            .partition_point(|sst| sst.first_key().as_key_slice() <= key)
            .saturating_sub(1);
        if idx >= sstables.len() {
            // key not found
            return Ok(Self {
                current: None,
                next_sst_idx: sstables.len(),
                sstables,
            });
        }
        let mut iter = Self {
            current: Some(SsTableIterator::create_and_seek_to_key(
                sstables[idx].clone(),
                key,
            )?),
            next_sst_idx: idx + 1,
            sstables,
        };

        iter.move_until_valid()?;

        Ok(iter)
    }

    fn check_sst_valid(ssts: &[Arc<SsTable>]) {
        // check key sort
        for sst in ssts {
            assert!(sst.first_key() <= sst.last_key())
        }
        if !ssts.is_empty() {
            for i in 0..(ssts.len() - 1) {
                assert!(ssts[i].last_key() < ssts[i + 1].first_key());
            }
        }
    }

    fn move_until_valid(&mut self) -> Result<()> {
        loop {
            if let Some(current) = &self.current.as_mut() {
                if current.is_valid() {
                    break;
                }
                if self.next_sst_idx < self.sstables.len() {
                    // move to next sst
                    let sst = self.sstables[self.next_sst_idx].clone();
                    let iter = SsTableIterator::create_and_seek_to_first(sst)?;
                    self.current = Some(iter);
                    self.next_sst_idx += 1;
                } else {
                    // no more sst
                    self.current = None
                }
            } else {
                break;
            }
        }

        Ok(())
    }
}

impl StorageIterator for SstConcatIterator {
    type KeyType<'a> = KeySlice<'a>;

    fn key(&self) -> KeySlice {
        // assume there is current
        self.current.as_ref().unwrap().key()
    }

    fn value(&self) -> &[u8] {
        // assume there is current
        self.current.as_ref().unwrap().value()
    }

    fn is_valid(&self) -> bool {
        if let Some(current) = &self.current {
            assert!(current.is_valid());
            true
        } else {
            // if no current, invalid
            false
        }
    }

    fn next(&mut self) -> Result<()> {
        self.current.as_mut().unwrap().next()?;
        self.move_until_valid()?;

        Ok(())
    }

    fn num_active_iterators(&self) -> usize {
        1
    }
}
