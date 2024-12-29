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
        let mut iter = Self {
            current: Some(SsTableIterator::create_and_seek_to_first(sstables[0].clone()).unwrap()),
            next_sst_idx: 1,
            sstables
        };

        iter.move_until_valid()?;

        Ok(iter)
    }

    pub fn create_and_seek_to_key(sstables: Vec<Arc<SsTable>>, key: KeySlice) -> Result<Self> {
        let mut iter = Self {
            current: Some(SsTableIterator::create_and_seek_to_key(sstables[0].clone(), key).unwrap()),
            next_sst_idx: 1,
            sstables
        };

        iter.move_until_valid()?;

        Ok(iter)
    } 

    fn move_until_valid(&mut self) -> Result<()> {
        loop {
            if let Some(current) = &self.current.as_mut() {
                if current.is_valid() {
                    break;
                }
                if !current.is_valid() {
                    if self.next_sst_idx < self.sstables.len() {
                        let sst = self.sstables[self.next_sst_idx].clone();
                        let iter = SsTableIterator::create_and_seek_to_first(sst)?;
                        self.current = Some(iter);
                        self.next_sst_idx += 1;
                    } else {
                        self.current = None
                    }
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
            self.current.as_ref().unwrap().is_valid()
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
