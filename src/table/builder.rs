#![allow(unused_variables)] // TODO(you): remove this lint after implementing this mod
#![allow(dead_code)] // TODO(you): remove this lint after implementing this mod

use std::sync::Arc;
use std::{mem, path::Path};

use anyhow::Result;
use bytes::{BufMut, Bytes};

use super::bloom::Bloom;
use super::{BlockMeta, FileObject, SsTable};
use crate::block::Block;
use crate::key::{KeyBytes, KeyVec};
use crate::{block::BlockBuilder, key::KeySlice, lsm_storage::BlockCache};

/// Builds an SSTable from key-value pairs.
pub struct SsTableBuilder {
    builder: BlockBuilder,
    first_key: Vec<u8>,
    last_key: Vec<u8>,
    data: Vec<u8>,
    pub(crate) meta: Vec<BlockMeta>,
    block_size: usize,
    key_hashes: Vec<u32>,
}

impl SsTableBuilder {
    /// Create a builder based on target block size.
    pub fn new(block_size: usize) -> Self {
        Self {
            builder: BlockBuilder::new(block_size),
            first_key: Vec::new(),
            last_key: Vec::new(),
            data: Vec::new(),
            meta: Vec::new(),
            block_size,
            key_hashes: Vec::new(),
        }
    }

    /// Adds a key-value pair to SSTable.
    ///
    /// Note: You should split a new block when the current block is full.(`std::mem::replace` may
    /// be helpful here)
    pub fn add(&mut self, key: KeySlice, value: &[u8]) {
        if self.first_key.is_empty() {
            self.first_key.clear();
            self.first_key.extend(key.raw_ref());
        }
        // hash key
        let key_hash = farmhash::fingerprint32(key.raw_ref());
        // stoer key hash
        self.key_hashes.push(key_hash);

        if self.builder.add(key, value) {
            self.last_key.clear();
            self.last_key.extend(key.raw_ref());
            return;
        }

        self.finish_block();

        // add again
        assert!(self.builder.add(key, value));
        // clear key
        self.first_key.clear();
        self.first_key.extend(key.raw_ref());
        self.last_key.clear();
        self.last_key.extend(key.raw_ref());
    }

    fn finish_block(&mut self) {
        // split a new block
        let new_builder = BlockBuilder::new(self.block_size);
        let old_builder = std::mem::replace(&mut self.builder, new_builder);
        // data
        let block = old_builder.build().encode();
        // add block meta
        let meta = BlockMeta {
            offset: self.data.len(),
            first_key: KeyVec::from_vec(std::mem::take(&mut self.first_key).into())
                .into_key_bytes(),
            last_key: KeyVec::from_vec(std::mem::take(&mut self.last_key).into()).into_key_bytes(),
        };
        self.meta.push(meta);
        self.data.extend(block);
    }

    /// Get the estimated size of the SSTable.
    ///
    /// Since the data blocks contain much more data than meta blocks, just return the size of data
    /// blocks here.
    pub fn estimated_size(&self) -> usize {
        self.data.len()
    }

    /// Builds the SSTable and writes it to the given path. Use the `FileObject` structure to manipulate the disk objects.
    pub fn build(
        mut self,
        id: usize,
        block_cache: Option<Arc<BlockCache>>,
        path: impl AsRef<Path>,
    ) -> Result<SsTable> {
        self.finish_block();
        let mut buf = self.data;
        let meta_offset = buf.len();
        BlockMeta::encode_block_meta(&self.meta, &mut buf);
        buf.put_u32(meta_offset as u32);
        // build bloom filter
        let bits_per_key = Bloom::bloom_bits_per_key(self.key_hashes.len(), 0.01);
        let bloom = Bloom::build_from_key_hashes(&self.key_hashes, bits_per_key);
        // append bloom to the end of sst
        let bloom_offset = buf.len();
        bloom.encode(&mut buf);
        buf.put_u32(bloom_offset as u32);
        // cerate sst file
        let file = FileObject::create(path.as_ref(), buf)?;

        Ok(SsTable {
            file,
            block_meta_offset: meta_offset,
            id,
            block_cache,
            first_key: self.meta.first().unwrap().first_key.clone(),
            last_key: self.meta.last().unwrap().last_key.clone(),
            bloom: Some(bloom),
            max_ts: 0,
            block_meta: self.meta,
        })
    }

    #[cfg(test)]
    pub(crate) fn build_for_test(self, path: impl AsRef<Path>) -> Result<SsTable> {
        self.build(0, None, path)
    }
}
