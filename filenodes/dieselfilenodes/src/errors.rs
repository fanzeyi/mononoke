// Copyright (c) 2018-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

pub use failure::{Error, Result};
use mercurial_types::RepoPath;

#[derive(Debug, Eq, Fail, PartialEq)]
pub enum ErrorKind {
    #[fail(display = "Invalid copy: {:?} copied from {:?}", _0, _1)]
    InvalidCopy(RepoPath, RepoPath),
    #[fail(display = "Internal error: path with hash {:?} not found", _0)] PathNotFound(Vec<u8>),
}