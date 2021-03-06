// Copyright (c) 2004-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

extern crate bookmarks;
#[macro_use]
extern crate failure_ext as failure;
extern crate futures;
extern crate futures_ext;
#[macro_use]
extern crate slog;

extern crate blobrepo;
extern crate mercurial_types;
extern crate metaconfig;
extern crate revset;

use std::sync::Arc;

use blobrepo::BlobRepo;
use bookmarks::Bookmark;
use futures::{Future, IntoFuture, Stream};
use futures_ext::{BoxFuture, FutureExt};
use mercurial_types::{Changeset, HgChangesetId, MPath, RepoPath};
use mercurial_types::manifest::{Entry, Type};
use mercurial_types::manifest_utils::recursive_entry_stream;
use metaconfig::CacheWarmupParams;
use revset::AncestorsNodeStream;
use slog::Logger;

mod errors {
    use bookmarks::Bookmark;

    #[derive(Debug, Fail)]
    pub enum ErrorKind {
        #[fail(display = "Bookmark {} does not exist", _0)] BookmarkNotFound(Bookmark),
    }
}

use failure::Error;

// Fetches all the manifest entries and their linknodes. Do not fetching files because
// there can be too many of them.
fn blobstore_and_filenodes_warmup(
    repo: Arc<BlobRepo>,
    revision: HgChangesetId,
    logger: Logger,
) -> BoxFuture<(), Error> {
    // TODO(stash): Arbitrary number. Tweak somehow?
    let buffer_size = 100;
    repo.get_changeset_by_changesetid(&revision)
        .map({
            let repo = repo.clone();
            move |cs| repo.get_root_entry(&cs.manifestid())
        })
        .and_then({
            move |root_entry| {
                info!(logger, "starting precaching");
                let rootpath = None;
                let mut i = 0;
                recursive_entry_stream(rootpath, root_entry)
                    .filter(|&(ref _path, ref entry)| entry.get_type() == Type::Tree)
                    .map(move |(path, entry)| {
                        let hash = entry.get_hash();
                        let path = MPath::join_element_opt(path.as_ref(), entry.get_name());
                        let path = match path {
                            Some(path) => RepoPath::DirectoryPath(path),
                            None => RepoPath::RootPath,
                        };
                        repo.get_linknode(path, &hash.into_nodehash())
                    })
                    .buffered(buffer_size)
                    .for_each(move |_| {
                        i += 1;
                        if i % 10000 == 0 {
                            debug!(logger, "fetched {}th entry during precaching", i);
                        }
                        Ok(())
                    })
            }
        })
        .boxify()
}

// Iterate over first parents, and fetch them
fn changesets_warmup(
    start_rev: HgChangesetId,
    repo: Arc<BlobRepo>,
    cs_limit: usize,
    logger: Logger,
) -> BoxFuture<(), Error> {
    info!(logger, "about to start warming up changesets cache");

    AncestorsNodeStream::new(&repo, start_rev.into_nodehash())
        .take(cs_limit as u64)
        .collect()
        .map(|_| ())
        .boxify()
}

fn do_cache_warmup(
    repo: Arc<BlobRepo>,
    bookmark: Bookmark,
    commit_limit: usize,
    logger: Logger,
) -> BoxFuture<(), Error> {
    repo.get_bookmark(&bookmark)
        .and_then({
            let logger = logger.clone();
            let repo = repo.clone();
            move |bookmark_rev| match bookmark_rev {
                Some(bookmark_rev) => {
                    let blobstore_warmup =
                        blobstore_and_filenodes_warmup(repo.clone(), bookmark_rev, logger.clone());
                    let cs_warmup =
                        changesets_warmup(bookmark_rev, repo, commit_limit, logger).boxify();
                    blobstore_warmup.join(cs_warmup).map(|_| ()).boxify()
                }
                None => {
                    info!(logger, "{} bookmark not found!", bookmark);
                    Err(errors::ErrorKind::BookmarkNotFound(bookmark).into())
                        .into_future()
                        .boxify()
                }
            }
        })
        .map(move |()| {
            info!(logger, "finished initial warmup");
            ()
        })
        .boxify()
}

/// Fetch all manifest entries for a bookmark, and fetches up to `commit_warmup_limit`
/// ancestors of the bookmark.
pub fn cache_warmup(
    repo: Arc<BlobRepo>,
    cache_warmup: Option<CacheWarmupParams>,
    logger: Logger,
) -> BoxFuture<(), Error> {
    match cache_warmup {
        Some(cache_warmup) => do_cache_warmup(
            repo,
            cache_warmup.bookmark,
            cache_warmup.commit_limit,
            logger.clone(),
        ),
        None => Ok(()).into_future().boxify(),
    }
}
