// Copyright (c) 2017-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

use std::collections::{BinaryHeap, HashSet};
use std::hash::Hash;

pub struct UniqueHeap<T>
where
    T: Ord + Hash + Eq,
{
    sorted_vals: BinaryHeap<T>,
    unique_vals: HashSet<T>,
}

impl<T> UniqueHeap<T>
where
    T: Ord + Hash + Eq + Clone,
{
    pub fn new() -> Self {
        UniqueHeap {
            sorted_vals: BinaryHeap::new(),
            unique_vals: HashSet::new(),
        }
    }

    pub fn push(&mut self, val: T) {
        if !self.unique_vals.contains(&val) {
            self.unique_vals.insert(val.clone());
            self.sorted_vals.push(val.clone());
        }
    }

    pub fn pop(&mut self) -> Option<T> {
        if let Some(v) = self.sorted_vals.pop() {
            self.unique_vals.remove(&v);
            Some(v)
        } else {
            None
        }
    }
}
