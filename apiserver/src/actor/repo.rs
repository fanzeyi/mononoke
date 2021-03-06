// Copyright (c) 2018-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

use std::convert::TryInto;
use std::sync::Arc;

use actix::{Actor, Context, Handler};
use failure::{err_msg, Error, Result};
use futures::{Future, IntoFuture};
use futures::sync::oneshot;
use futures_ext::BoxFuture;
use slog::Logger;
use tokio::runtime::TaskExecutor;

use api;
use blobrepo::BlobRepo;
use futures_ext::FutureExt;
use mercurial_types::RepositoryId;
use mercurial_types::manifest::Content;
use metaconfig::repoconfig::RepoConfig;
use metaconfig::repoconfig::RepoType::{BlobManifold, BlobRocks};
use mononoke_types::FileContents;
use reachabilityindex::{GenerationNumberBFS, ReachabilityIndex};

use errors::ErrorKind;
use from_string as FS;

use super::{MononokeRepoQuery, MononokeRepoResponse};
use super::model::Entry;

pub struct MononokeRepoActor {
    repo: Arc<BlobRepo>,
    logger: Logger,
    executor: TaskExecutor,
}

impl MononokeRepoActor {
    pub fn new(logger: Logger, config: RepoConfig, executor: TaskExecutor) -> Result<Self> {
        let repoid = RepositoryId::new(config.repoid);
        let repo = match config.repotype {
            BlobRocks(ref path) => BlobRepo::new_rocksdb(logger.clone(), &path, repoid),
            BlobManifold { ref args, .. } => BlobRepo::new_manifold(logger.clone(), args, repoid),
            _ => Err(err_msg("Unsupported repo type.")),
        };

        repo.map(|repo| Self {
            repo: Arc::new(repo),
            logger: logger,
            executor: executor,
        })
    }

    fn get_raw_file(
        &self,
        changeset: String,
        path: String,
    ) -> Result<BoxFuture<MononokeRepoResponse, Error>> {
        debug!(
            self.logger,
            "Retrieving file content of {} at changeset {}.", path, changeset
        );

        let mpath = FS::get_mpath(path.clone())?;
        let changesetid = FS::get_changeset_id(changeset)?;
        let repo = self.repo.clone();

        Ok(api::get_content_by_path(repo, changesetid, Some(mpath))
            .and_then(move |content| match content {
                Content::File(content)
                | Content::Executable(content)
                | Content::Symlink(content) => Ok(MononokeRepoResponse::GetRawFile {
                    content: content.into_bytes(),
                }),
                _ => Err(ErrorKind::InvalidInput(path.to_string(), None).into()),
            })
            .from_err()
            .boxify())
    }

    fn is_ancestor(
        &self,
        proposed_ancestor: String,
        proposed_descendent: String,
    ) -> Result<BoxFuture<MononokeRepoResponse, Error>> {
        let genbfs = GenerationNumberBFS::new();
        let src_hash_maybe = FS::get_nodehash(&proposed_descendent);
        let dst_hash_maybe = FS::get_nodehash(&proposed_ancestor);
        let src_hash_future = src_hash_maybe.into_future().or_else({
            cloned!(self.repo);
            move |_| {
                FS::string_to_bookmark_changeset_id(proposed_descendent, repo)
                    .map(|node_cs| *node_cs.as_nodehash())
            }
        });
        let dst_hash_future = dst_hash_maybe.into_future().or_else({
            cloned!(self.repo);
            move |_| {
                FS::string_to_bookmark_changeset_id(proposed_ancestor, repo)
                    .map(|node_cs| *node_cs.as_nodehash())
            }
        });

        let (tx, rx) = oneshot::channel::<Result<bool>>();

        self.executor.spawn(
            src_hash_future
                .and_then(|src| dst_hash_future.map(move |dst| (src, dst)))
                .and_then({
                    cloned!(self.repo);
                    move |(src, dst)| genbfs.query_reachability(repo, src, dst)
                })
                .then(|r| tx.send(r).map_err(|_| ())),
        );

        Ok(rx.flatten()
            .map(|answer| MononokeRepoResponse::IsAncestor { answer })
            .from_err()
            .boxify())
    }

    fn get_blob_content(&self, hash: String) -> Result<BoxFuture<MononokeRepoResponse, Error>> {
        let blobhash = FS::get_nodehash(&hash)?;

        Ok(self.repo
            .get_file_content(&blobhash)
            .and_then(move |content| match content {
                FileContents::Bytes(content) => {
                    Ok(MononokeRepoResponse::GetBlobContent { content })
                }
            })
            .from_err()
            .boxify())
    }

    fn list_directory(
        &self,
        changeset: String,
        path: String,
    ) -> Result<BoxFuture<MononokeRepoResponse, Error>> {
        let mpath = if path.is_empty() {
            None
        } else {
            Some(FS::get_mpath(path.clone())?)
        };
        let changesetid = FS::get_changeset_id(changeset)?;
        let repo = self.repo.clone();

        Ok(api::get_content_by_path(repo, changesetid, mpath)
            .and_then(move |content| match content {
                Content::Tree(tree) => Ok(tree),
                _ => Err(ErrorKind::InvalidInput(path.to_string(), None).into()),
            })
            .map(|tree| {
                tree.list()
                    .filter_map(|entry| -> Option<Entry> { entry.try_into().ok() })
            })
            .map(|files| MononokeRepoResponse::ListDirectory {
                files: Box::new(files),
            })
            .from_err()
            .boxify())
    }

    fn get_tree(&self, hash: String) -> Result<BoxFuture<MononokeRepoResponse, Error>> {
        let treehash = FS::get_nodehash(&hash)?;

        Ok(self.repo
            .get_manifest_by_nodeid(&treehash)
            .map(|tree| {
                tree.list()
                    .filter_map(|entry| -> Option<Entry> { entry.try_into().ok() })
            })
            .map(|files| MononokeRepoResponse::GetTree {
                files: Box::new(files),
            })
            .from_err()
            .boxify())
    }

    fn get_changeset(&self, hash: String) -> Result<BoxFuture<MononokeRepoResponse, Error>> {
        let changesetid = FS::get_changeset_id(hash)?;

        Ok(self.repo
            .get_changeset_by_changesetid(&changesetid)
            .and_then(|changeset| changeset.try_into().map_err(From::from))
            .map(|changeset| MononokeRepoResponse::GetChangeset { changeset })
            .from_err()
            .boxify())
    }
}

impl Actor for MononokeRepoActor {
    type Context = Context<Self>;
}

impl Handler<MononokeRepoQuery> for MononokeRepoActor {
    type Result = Result<BoxFuture<MononokeRepoResponse, Error>>;

    fn handle(&mut self, msg: MononokeRepoQuery, _ctx: &mut Context<Self>) -> Self::Result {
        use MononokeRepoQuery::*;

        match msg {
            GetRawFile { changeset, path } => self.get_raw_file(changeset, path),
            GetBlobContent { hash } => self.get_blob_content(hash),
            ListDirectory { changeset, path } => self.list_directory(changeset, path),
            GetTree { hash } => self.get_tree(hash),
            GetChangeset { hash } => self.get_changeset(hash),
            IsAncestor {
                proposed_ancestor,
                proposed_descendent,
            } => self.is_ancestor(proposed_ancestor, proposed_descendent),
        }
    }
}
