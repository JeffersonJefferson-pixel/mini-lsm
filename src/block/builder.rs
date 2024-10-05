#![allow(unused_variables)] // TODO(you): remove this lint after implementing this mod
#![allow(dead_code)] // TODO(you): remove this lint after implementing this mod

use bytes::BufMut;

use crate::key::{Key, KeySlice, KeyVec};

use super::{Block, SIZEOF_U16};

/// Builds a block.
pub struct BlockBuilder {
    /// Offsets of each key-value entries.
    offsets: Vec<u16>,
    /// All serialized key-value pairs in the block.
    data: Vec<u8>,
    /// The expected block size.
    block_size: usize,
    /// The first key in the block
    first_key: KeyVec,
}

impl BlockBuilder {
    /// Creates a new block builder.
    pub fn new(block_size: usize) -> Self {
        Self {
            offsets: Vec::new(),
            data: Vec::new(),
            block_size,
            first_key: Key::new(),
        }
    }

    fn size(&self) -> usize {
        // offsets + data + number of entries
        self.offsets.len() * SIZEOF_U16 + self.data.len() + SIZEOF_U16
    }

    /// Adds a key-value pair to the block. Returns false when the block is full.
    #[must_use]
    pub fn add(&mut self, key: KeySlice, value: &[u8]) -> bool {
        // validate key
        assert!(!key.is_empty(), "key must not be empty");
        // key len + key + value len + value + offset
        let entry_size = SIZEOF_U16 + key.len() + SIZEOF_U16 + value.len() + SIZEOF_U16;
        // check size
        if self.size() + entry_size > self.block_size && !self.is_empty() {
            return false;
        }
        // offset
        self.offsets.push(self.data.len() as u16);
        // key
        self.data.put_u16(key.len() as u16);
        self.data.put(key.into_inner());
        // value
        self.data.put_u16(value.len() as u16);
        self.data.put(value);

        true
    }

    /// Check if there is no key-value pair in the block.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Finalize the block.
    pub fn build(self) -> Block {
        Block {
            data: self.data,
            offsets: self.offsets,
        }
    }
}
