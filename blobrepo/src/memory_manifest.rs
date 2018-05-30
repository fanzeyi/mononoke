// Copyright (c) 2004-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

//! In memory manifests, used to convert Bonsai Changesets to old style

use std::collections::BTreeMap;
use std::fmt::{self, Debug};
use std::io::Write;
use std::sync::{Arc, Mutex};

use futures::future::{self, Future, IntoFuture};
use futures::stream::{self, Stream};
use futures_ext::{BoxFuture, FutureExt};

use slog::Logger;

use mercurial::{HgNodeHash, NodeHashConversion};
use mercurial_types::{DManifestId, Entry, MPath, MPathElement, Manifest, RepoPath, Type};

use blobstore::Blobstore;
use file::HgBlobEntry;
use repo::{UploadHgEntry, UploadHgNodeHash};

use errors::*;
use manifest::BlobManifest;

/// An in-memory manifest entry. Clones are *not* separate - they share a single set of changes.
/// This is because futures require ownership, and I don't want to Arc all of this when there's
/// only a small amount of changing data.
#[derive(Clone)]
enum MemoryManifestEntry {
    /// A blob already found in the blob store. This cannot be a Tree blob
    Blob(HgBlobEntry),
    /// There are conflicting options here, to be resolved
    /// The vector contains each of the conflicting manifest entries, for use in generating
    /// parents of the final entry when bonsai changeset resolution removes this conflict
    Conflict(Vec<MemoryManifestEntry>),
    /// This entry is an in-memory tree, and will need writing out to finish
    /// resolving the manifests
    MemTree {
        base_manifest_id: Option<HgNodeHash>,
        p1: Option<HgNodeHash>,
        p2: Option<HgNodeHash>,
        changes: Arc<Mutex<BTreeMap<MPathElement, Option<MemoryManifestEntry>>>>,
    },
}

impl Debug for MemoryManifestEntry {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match self {
            &MemoryManifestEntry::Blob(ref blob) => {
                fmt.debug_tuple("Blob hash").field(blob.get_hash()).finish()
            }
            &MemoryManifestEntry::Conflict(ref conflicts) => {
                fmt.debug_list().entries(conflicts.iter()).finish()
            }
            &MemoryManifestEntry::MemTree {
                ref base_manifest_id,
                ref p1,
                ref p2,
                ref changes,
            } => {
                let changes = changes.lock().expect("lock poisoned");
                fmt.debug_struct("MemTree")
                    .field("base_manifest_id", base_manifest_id)
                    .field("p1", p1)
                    .field("p2", p2)
                    .field("changes", &*changes)
                    .finish()
            }
        }
    }
}

// This is tied to the implementation of MemoryManifestEntry::save below
fn extend_repopath_with_dir(path: &RepoPath, dir: &MPathElement) -> RepoPath {
    assert!(path.is_dir() || path.is_root(), "Cannot extend a filepath");

    let opt_mpath = MPath::join_opt(path.mpath(), dir);
    match opt_mpath {
        None => RepoPath::root(),
        Some(p) => RepoPath::dir(p).expect("Can't convert an MPath to an MPath?!?"),
    }
}

impl MemoryManifestEntry {
    /// True if this entry is a tree with no children
    fn is_empty(&self, blobstore: &Arc<Blobstore>) -> BoxFuture<bool, Error> {
        match self {
            &MemoryManifestEntry::MemTree { .. } => self.get_new_children(blobstore)
                .and_then({
                    let blobstore = blobstore.clone();
                    move |children| {
                        future::join_all(
                            children
                                .into_iter()
                                .map(move |(_, child)| child.is_empty(&blobstore)),
                        )
                    }
                })
                .map(|f| f.into_iter().all(|ce| ce))
                .boxify(),
            _ => future::ok(false).boxify(),
        }
    }

    /// True if this entry is a Tree, false otherwise
    #[cfg(test)]
    pub fn is_dir(&self) -> bool {
        match self {
            &MemoryManifestEntry::MemTree { .. } => true,
            _ => false,
        }
    }

    /// Get an empty tree manifest entry
    pub fn empty_tree() -> Self {
        MemoryManifestEntry::MemTree {
            base_manifest_id: None,
            p1: None,
            p2: None,
            changes: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// True if there's been any modification to self, false if not a MemTree or unmodified
    fn is_modified(&self) -> bool {
        if let &MemoryManifestEntry::MemTree {
            ref base_manifest_id,
            ref changes,
            ..
        } = self
        {
            // We are definitionally modified if there's no baseline,
            // even if we're actually empty
            let changes = changes.lock().expect("lock poisoned");
            base_manifest_id.is_none() || !changes.is_empty()
        } else {
            false
        }
    }

    /// Save all manifests represented here to the blobstore
    pub fn save(
        &self,
        blobstore: &Arc<Blobstore>,
        logger: &Logger,
        path: RepoPath,
    ) -> BoxFuture<HgBlobEntry, Error> {
        match self {
            &MemoryManifestEntry::Blob(ref blob) => future::ok(blob.clone()).boxify(),
            &MemoryManifestEntry::Conflict(_) => {
                future::err(ErrorKind::UnresolvedConflicts.into()).boxify()
            }
            &MemoryManifestEntry::MemTree {
                base_manifest_id,
                p1,
                p2,
                ..
            } => {
                if self.is_modified() {
                    self.get_new_children(blobstore)
                        .and_then({
                            let logger = logger.clone();
                            let blobstore = blobstore.clone();

                            move |new_children| {
                                // First save only the non-empty children
                                let entries = stream::iter_ok(new_children.into_iter())
                                    .and_then({
                                        let logger = logger.clone();
                                        let blobstore = blobstore.clone();
                                        let path = path.clone();
                                        move |(path_elem, entry)| {
                                            let path_elem = path_elem.clone();
                                            // This is safe, because we only save trees
                                            let entry_path =
                                                extend_repopath_with_dir(&path, &path_elem);
                                            entry.is_empty(&blobstore).and_then({
                                                let logger = logger.clone();
                                                let blobstore = blobstore.clone();
                                                move |empty| {
                                                    if empty {
                                                        None
                                                    } else {
                                                        Some(
                                                            entry
                                                                .save(
                                                                    &blobstore,
                                                                    &logger,
                                                                    entry_path,
                                                                )
                                                                .map(move |entry| {
                                                                    (path_elem, entry)
                                                                }),
                                                        )
                                                    }
                                                }
                                            })
                                        }
                                    })
                                    .filter_map(|i| i)
                                    .collect();

                                // Then write out a manifest for this tree node
                                entries.and_then({
                                    let blobstore = blobstore.clone();
                                    let logger = logger.clone();
                                    move |entries| {
                                        let mut manifest: Vec<u8> = Vec::new();
                                        entries.iter().for_each(|&(ref path, ref entry)| {
                                            manifest.extend(path.as_bytes());
                                            // Ignoring errors writing to memory
                                            let _ = write!(
                                                &mut manifest,
                                                "\0{}{}\n",
                                                entry.get_hash().into_nodehash(),
                                                entry.get_type(),
                                            );
                                        });

                                        let upload_entry = UploadHgEntry {
                                            upload_nodeid: UploadHgNodeHash::Generate,
                                            raw_content: manifest.into(),
                                            content_type: Type::Tree,
                                            p1,
                                            p2,
                                            path,
                                        };

                                        let (_hash, future) = try_boxfuture!(
                                            upload_entry.upload_to_blobstore(&blobstore, &logger)
                                        );
                                        future.map(|(entry, _path)| entry).boxify()
                                    }
                                })
                            }
                        })
                        .boxify()
                } else {
                    if p2.is_some() {
                        future::err(ErrorKind::UnchangedManifest.into()).boxify()
                    } else {
                        let blobstore = blobstore.clone();
                        future::result(base_manifest_id.ok_or(ErrorKind::UnchangedManifest.into()))
                            .and_then(move |base_manifest_id| {
                                match path.mpath().map(MPath::basename) {
                                    None => future::ok(HgBlobEntry::new_root(
                                        blobstore,
                                        DManifestId::new(base_manifest_id.into_mononoke()),
                                    )),
                                    Some(path) => future::ok(HgBlobEntry::new(
                                        blobstore,
                                        path.clone(),
                                        base_manifest_id.into_mononoke(),
                                        Type::Tree,
                                    )),
                                }
                            })
                            .boxify()
                    }
                }
            }
        }
    }

    fn apply_changes(
        changes: Arc<Mutex<BTreeMap<MPathElement, Option<Self>>>>,
        mut children: BTreeMap<MPathElement, Self>,
    ) -> BTreeMap<MPathElement, Self> {
        let changes = changes.lock().expect("lock poisoned");
        for (path, entry) in changes.iter() {
            match entry {
                &None => {
                    children.remove(path);
                }
                &Some(ref new) => {
                    children.insert(path.clone(), new.clone());
                }
            }
        }
        children
    }

    // The list of this node's children, or empty if it's not a tree with children.
    fn get_new_children(
        &self,
        blobstore: &Arc<Blobstore>,
    ) -> BoxFuture<BTreeMap<MPathElement, Self>, Error> {
        match self {
            &MemoryManifestEntry::MemTree {
                ref base_manifest_id,
                ref changes,
                ..
            } => match base_manifest_id {
                &Some(ref manifest_id) => BlobManifest::load(
                    blobstore,
                    &DManifestId::new(manifest_id.into_mononoke()),
                ).and_then({
                    let manifest_id = manifest_id.into_mononoke();
                    move |m| future::result(m.ok_or(ErrorKind::ManifestMissing(manifest_id).into()))
                })
                    .and_then({
                        let blobstore = blobstore.clone();
                        move |m| {
                            m.list()
                                .and_then(move |entry| {
                                    let name = entry
                                        .get_name()
                                        .expect("Unnamed entry in a manifest")
                                        .clone();
                                    match entry.get_type() {
                                        Type::Tree => future::ok(Self::convert_treenode(&entry
                                            .get_hash()
                                            .into_nodehash()
                                            .into_mercurial()))
                                            .boxify(),
                                        _ => future::ok(MemoryManifestEntry::Blob(
                                            HgBlobEntry::new(
                                                blobstore.clone(),
                                                name.clone(),
                                                entry.get_hash().into_nodehash(),
                                                entry.get_type(),
                                            ),
                                        )).boxify(),
                                    }.map(move |entry| (name, entry))
                                        .boxify()
                                })
                                .fold(BTreeMap::new(), move |mut children, (name, entry)| {
                                    children.insert(name, entry);
                                    future::ok::<_, Error>(children)
                                })
                        }
                    })
                    .map({
                        let changes = changes.clone();
                        move |children| Self::apply_changes(changes, children)
                    })
                    .boxify(),
                // No baseline manifest - take an empty starting point.
                &None => future::ok(Self::apply_changes(changes.clone(), BTreeMap::new())).boxify(),
            },
            _ => future::ok(BTreeMap::new()).boxify(),
        }
    }

    pub fn convert_treenode(manifest_id: &HgNodeHash) -> Self {
        MemoryManifestEntry::MemTree {
            base_manifest_id: Some(*manifest_id),
            p1: Some(*manifest_id),
            p2: None,
            changes: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    fn merge_trees(
        mut children: BTreeMap<MPathElement, MemoryManifestEntry>,
        other_children: BTreeMap<MPathElement, MemoryManifestEntry>,
        blobstore: Arc<Blobstore>,
        logger: Logger,
        repo_path: RepoPath,
        p1: Option<HgNodeHash>,
        p2: Option<HgNodeHash>,
    ) -> impl Future<Item = MemoryManifestEntry, Error = Error> + Send {
        let mut conflicts = stream::FuturesUnordered::new();

        for (path, other_entry) in other_children.into_iter() {
            match children.remove(&path) {
                None => {
                    // Only present in other - take their version.
                    children.insert(path, other_entry);
                }
                Some(conflict_entry) => {
                    // This is safe, because we only save trees to fix conflicts
                    let repo_path = extend_repopath_with_dir(&repo_path, &path);

                    // Remember the conflict for processing later
                    conflicts.push(
                        conflict_entry
                            .merge_with_conflicts(
                                other_entry,
                                blobstore.clone(),
                                logger.clone(),
                                repo_path,
                            )
                            .map(move |entry| (path, entry)),
                    );
                }
            }
        }

        // Add all the handled conflicts to a MemoryManifestEntry and then make them into a new
        // entry
        conflicts.collect().map(move |conflicts| {
            children.extend(conflicts.into_iter());
            MemoryManifestEntry::MemTree {
                base_manifest_id: None,
                p1,
                p2,
                changes: Arc::new(Mutex::new(
                    children
                        .into_iter()
                        .map(|(path, entry)| (path, Some(entry)))
                        .collect(),
                )),
            }
        })
    }
    /// Merge two MemoryManifests together, tracking conflicts. Conflicts are put in the data
    /// structure in strict order, so that first entry is p1, second is p2 etc.
    pub fn merge_with_conflicts(
        self,
        other: Self,
        blobstore: Arc<Blobstore>,
        logger: Logger,
        repo_path: RepoPath,
    ) -> BoxFuture<Self, Error> {
        use self::MemoryManifestEntry::*;
        if self.is_modified() {
            return self.save(&blobstore, &logger, repo_path.clone())
                .map(|entry| {
                    Self::convert_treenode(&entry.get_hash().into_nodehash().into_mercurial())
                })
                .and_then(move |saved| {
                    saved.merge_with_conflicts(other, blobstore, logger, repo_path)
                })
                .boxify();
        }
        if other.is_modified() {
            return other
                .save(&blobstore, &logger, repo_path.clone())
                .map(|entry| {
                    Self::convert_treenode(&entry.get_hash().into_nodehash().into_mercurial())
                })
                .and_then(move |saved| {
                    self.merge_with_conflicts(saved, blobstore, logger, repo_path)
                })
                .boxify();
        }

        match (&self, &other) {
            // Conflicts (on either side) must be resolved before you merge
            (_, Conflict(_)) | (Conflict(_), _) => {
                future::err(ErrorKind::UnresolvedConflicts.into()).boxify()
            }
            // Two identical blobs merge to an unchanged blob
            (Blob(p1), Blob(p2)) if p1 == p2 => future::ok(self.clone()).boxify(),
            // Otherwise, blobs are in conflict - either another blob, or a tree
            (Blob(_), _) | (_, Blob(_)) => {
                future::ok(Conflict(vec![self.clone(), other.clone()])).boxify()
            }
            // If either tree is already a merge, we can't merge further
            (
                MemTree {
                    p1: Some(p1),
                    p2: Some(p2),
                    ..
                },
                _,
            )
            | (
                _,
                MemTree {
                    p1: Some(p1),
                    p2: Some(p2),
                    ..
                },
            ) => future::err(ErrorKind::ManifestAlreadyAMerge(*p1, *p2).into()).boxify(),
            (
                MemTree {
                    base_manifest_id: my_id,
                    p1,
                    changes: my_changes,
                    ..
                },
                MemTree {
                    base_manifest_id: other_id,
                    p1: p2,
                    changes: other_changes,
                    ..
                },
            ) => {
                let my_changes = my_changes.lock().expect("lock poisoned");
                let other_changes = other_changes.lock().expect("lock poisoned");
                // Two identical manifests, neither one modified
                if my_id.is_some() && my_id == other_id && my_changes.is_empty()
                    && other_changes.is_empty()
                {
                    future::ok(self.clone()).boxify()
                } else {
                    // Otherwise, merge on an entry-by-entry basis
                    self.get_new_children(&blobstore)
                        .join(other.get_new_children(&blobstore))
                        .and_then({
                            let p1 = p1.clone();
                            let p2 = p2.clone();
                            move |(children, other_children)| {
                                Self::merge_trees(
                                    children,
                                    other_children,
                                    blobstore,
                                    logger,
                                    repo_path,
                                    p1,
                                    p2,
                                )
                            }
                        })
                        .boxify()
                }
            }
        }
    }

    // Only for use in find_mut_helper
    fn conflict_to_memtree(&mut self) -> Self {
        let new = if let &mut MemoryManifestEntry::Conflict(ref conflicts) = self {
            let mut parents = conflicts
                .into_iter()
                .filter_map(|entry| match entry {
                    &MemoryManifestEntry::MemTree {
                        ref base_manifest_id,
                        ..
                    } if !entry.is_modified() =>
                    {
                        *base_manifest_id
                    }
                    &MemoryManifestEntry::Blob(ref blob) if blob.get_type() == Type::Tree => {
                        Some(blob.get_hash().into_nodehash().into_mercurial())
                    }
                    _ => None,
                })
                .fuse();
            Some(MemoryManifestEntry::MemTree {
                base_manifest_id: None,
                p1: parents.next(),
                p2: parents.next(),
                changes: Arc::new(Mutex::new(BTreeMap::new())),
            })
        } else {
            None
        };
        if let Some(new) = new {
            *self = new;
        }
        self.clone()
    }

    fn find_mut_helper<'a>(
        changes: &'a mut BTreeMap<MPathElement, Option<Self>>,
        path: MPathElement,
    ) -> Self {
        changes
            .entry(path)
            .or_insert(Some(Self::empty_tree()))
            .get_or_insert_with(Self::empty_tree)
            .conflict_to_memtree()
    }

    fn manifest_lookup(
        manifest: BlobManifest,
        entry_changes: Arc<Mutex<BTreeMap<MPathElement, Option<MemoryManifestEntry>>>>,
        element: MPathElement,
        blobstore: Arc<Blobstore>,
    ) -> impl Future<Item = (), Error = Error> {
        manifest.lookup(&element).map(move |entry| {
            if let Some(entry) = entry {
                let entry = match entry.get_type() {
                    Type::Tree => {
                        Self::convert_treenode(&entry.get_hash().into_nodehash().into_mercurial())
                    }
                    _ => MemoryManifestEntry::Blob(HgBlobEntry::new(
                        blobstore,
                        element.clone(),
                        entry.get_hash().into_nodehash(),
                        entry.get_type(),
                    )),
                };
                let mut changes = entry_changes.lock().expect("lock poisoned");
                changes.insert(element, Some(entry));
            }
        })
    }

    /// Creates directories as needed to find the element referred to by path
    /// This will be a tree if it's been freshly created, or whatever is in the manifest if it
    /// was present. Returns a None if the path cannot be created (e.g. there's a file part
    /// way through the path)
    pub fn find_mut<I>(
        &self,
        mut path: I,
        blobstore: Arc<Blobstore>,
    ) -> BoxFuture<Option<Self>, Error>
    where
        I: Iterator<Item = MPathElement> + Send + 'static,
    {
        match path.next() {
            None => future::ok(Some(self.clone())).boxify(),
            Some(element) => {
                // First check to see if I've already got an entry in changes (while locked),
                // and recurse into that entry
                // If not, lookup the entry
                // On fail, put an empty tree in changes
                // On success, put the lookup result in changes and retry
                match self {
                    &MemoryManifestEntry::MemTree {
                        ref base_manifest_id,
                        changes: ref entry_changes,
                        ..
                    } => {
                        let entry_changes = entry_changes.clone();
                        let element_known = {
                            let mut changes = entry_changes.lock().expect("lock poisoned");
                            changes.contains_key(&element)
                        };
                        if element_known {
                            future::ok(()).boxify()
                        } else {
                            // Do the lookup in base_manifest_id
                            if let &Some(ref manifest_id) = base_manifest_id {
                                BlobManifest::load(
                                    &blobstore,
                                    &DManifestId::new(manifest_id.into_mononoke()),
                                ).and_then({
                                    let manifest_id = manifest_id.into_mononoke();
                                    move |m| {
                                        future::result(m.ok_or(
                                            ErrorKind::ManifestMissing(manifest_id).into(),
                                        ))
                                    }
                                })
                                    .and_then({
                                        let entry_changes = entry_changes.clone();
                                        let element = element.clone();
                                        let blobstore = blobstore.clone();
                                        move |m| {
                                            Self::manifest_lookup(
                                                m,
                                                entry_changes,
                                                element,
                                                blobstore,
                                            )
                                        }
                                    })
                                    .boxify()
                            } else {
                                future::ok(()).boxify()
                            }
                        }.and_then(move |_| {
                            let mut changes = entry_changes.lock().expect("lock poisoned");
                            Self::find_mut_helper(&mut changes, element).find_mut(path, blobstore)
                        })
                            .boxify()
                    }
                    _ => future::ok(None).boxify(),
                }
            }
        }
    }

    /// Change an entry - remove if None, set if Some(entry)
    pub fn change(&self, element: MPathElement, change: Option<HgBlobEntry>) -> Result<()> {
        match self {
            &MemoryManifestEntry::MemTree { ref changes, .. } => {
                let mut changes = changes.lock().expect("lock poisoned");
                changes.insert(element, change.map(|c| MemoryManifestEntry::Blob(c)));
                Ok(())
            }
            _ => Err(ErrorKind::NotADirectory.into()),
        }
    }
}

/// An in memory manifest, created from parent manifests (if any)
pub struct MemoryRootManifest {
    blobstore: Arc<Blobstore>,
    root_entry: MemoryManifestEntry,
    logger: Logger,
}

impl MemoryRootManifest {
    fn create(blobstore: Arc<Blobstore>, root_entry: MemoryManifestEntry, logger: Logger) -> Self {
        Self {
            blobstore,
            root_entry,
            logger,
        }
    }

    fn create_conflict(
        blobstore: Arc<Blobstore>,
        p1_root: MemoryManifestEntry,
        p2_root: MemoryManifestEntry,
        logger: Logger,
    ) -> BoxFuture<Self, Error> {
        p1_root
            .merge_with_conflicts(p2_root, blobstore.clone(), logger.clone(), RepoPath::root())
            .map(move |root| Self::create(blobstore, root, logger))
            .boxify()
    }

    /// Create an in-memory manifest, backed by the given blobstore, and based on mp1 and mp2
    pub fn new(
        blobstore: Arc<Blobstore>,
        logger: Logger,
        mp1: Option<&HgNodeHash>,
        mp2: Option<&HgNodeHash>,
    ) -> BoxFuture<Self, Error> {
        match (mp1, mp2) {
            (None, None) => future::ok(Self::create(
                blobstore,
                MemoryManifestEntry::empty_tree(),
                logger,
            )).boxify(),
            (Some(p), None) | (None, Some(p)) => future::ok(Self::create(
                blobstore,
                MemoryManifestEntry::convert_treenode(p),
                logger,
            )).boxify(),
            (Some(p1), Some(p2)) => Self::create_conflict(
                blobstore,
                MemoryManifestEntry::convert_treenode(p1),
                MemoryManifestEntry::convert_treenode(p2),
                logger,
            ),
        }
    }

    /// Save this manifest to the blobstore, recursing down to ensure that
    /// all child entries are saved and that there are no conflicts.
    /// Note that child entries can be saved even if a parallel tree has conflicts. E.g. if the
    /// manifest contains dir1/file1 and dir2/file2 and dir2 contains a conflict for file2, dir1
    /// can still be saved to the blobstore.
    /// Returns the saved manifest ID
    pub fn save(&self) -> BoxFuture<HgBlobEntry, Error> {
        self.root_entry
            .save(&self.blobstore, &self.logger, RepoPath::root())
            .boxify()
    }

    fn find_path(&self, path: &MPath) -> (BoxFuture<MemoryManifestEntry, Error>, MPathElement) {
        let (possible_path, filename) = path.split_dirname();
        let target = match possible_path {
            None => future::ok(Some(self.root_entry.clone())).boxify(),
            Some(filepath) => self.root_entry
                .find_mut(filepath.into_iter(), self.blobstore.clone()),
        }.and_then({
            let path = path.clone();
            |entry| future::result(entry.ok_or(ErrorKind::PathNotFound(path).into()))
        })
            .boxify();

        (target, filename.clone())
    }

    /// Apply an add or remove based on whether the change is None (remove) or Some(blobentry) (add)
    pub fn change_entry(&self, path: &MPath, entry: Option<HgBlobEntry>) -> BoxFuture<(), Error> {
        let (target, filename) = self.find_path(path);

        target
            .and_then(|target| target.change(filename, entry).into_future())
            .boxify()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use async_unit;
    use many_files_dirs;
    use mercurial_types::{DNodeHash, FileType, nodehash::DEntryId};
    use mercurial_types_mocks::nodehash;
    use slog::Discard;

    fn insert_entry(tree: &MemoryManifestEntry, path: MPathElement, entry: MemoryManifestEntry) {
        match tree {
            &MemoryManifestEntry::MemTree { ref changes, .. } => {
                let mut changes = changes.lock().expect("lock poisoned");
                changes.insert(path, Some(entry));
            }
            _ => panic!("Inserting into a non-Tree"),
        }
    }

    #[test]
    fn empty_manifest() {
        async_unit::tokio_unit_test(|| {
            let blobstore = many_files_dirs::getrepo(None).get_blobstore();
            let logger = Logger::root(Discard, o![]);

            // Create an empty memory manifest
            let memory_manifest = MemoryRootManifest::new(blobstore, logger, None, None)
                .wait()
                .expect("Could not create empty manifest");

            if let MemoryManifestEntry::MemTree {
                base_manifest_id,
                p1,
                p2,
                changes,
            } = memory_manifest.root_entry
            {
                let changes = changes.lock().expect("lock poisoned");
                assert!(base_manifest_id.is_none(), "Empty manifest had a baseline");
                assert!(p1.is_none(), "Empty manifest had p1");
                assert!(p2.is_none(), "Empty manifest had p2");
                assert!(changes.is_empty(), "Empty manifest had new entries changed");
            } else {
                panic!("Empty manifest is not a MemTree");
            }
        })
    }

    #[test]
    fn load_manifest() {
        async_unit::tokio_unit_test(|| {
            let blobstore = many_files_dirs::getrepo(None).get_blobstore();
            let logger = Logger::root(Discard, o![]);

            let manifest_id = DNodeHash::from_static_str(
                "b267a6869fcc39b37741408b5823cc044233201d",
            ).expect("Could not get nodehash")
                .into_mercurial();

            // Load a memory manifest
            let memory_manifest =
                MemoryRootManifest::new(blobstore, logger, Some(&manifest_id), None)
                    .wait()
                    .expect("Could not load manifest");

            if let MemoryManifestEntry::MemTree {
                base_manifest_id,
                p1,
                p2,
                changes,
            } = memory_manifest.root_entry
            {
                let changes = changes.lock().expect("lock poisoned");
                assert_eq!(
                    base_manifest_id,
                    Some(manifest_id),
                    "Loaded manifest had wrong base {:?}",
                    base_manifest_id
                );
                assert_eq!(
                    p1,
                    Some(manifest_id),
                    "Loaded manifest had wrong p1 {:?}",
                    p1
                );
                assert!(p2.is_none(), "Loaded manifest had p2");
                assert!(
                    changes.is_empty(),
                    "Loaded (unaltered) manifest has had entries changed"
                );
            } else {
                panic!("Loaded manifest is not a MemTree");
            }
        })
    }

    #[test]
    fn save_manifest() {
        async_unit::tokio_unit_test(|| {
            let repo = many_files_dirs::getrepo(None);
            let blobstore = repo.get_blobstore();
            let logger = Logger::root(Discard, o![]);

            // Create an empty memory manifest
            let mut memory_manifest =
                MemoryRootManifest::new(blobstore.clone(), logger, None, None)
                    .wait()
                    .expect("Could not create empty manifest");

            // Add an unmodified entry
            let dir_nodehash = DNodeHash::from_static_str(
                "b267a6869fcc39b37741408b5823cc044233201d",
            ).expect("Could not get nodehash");
            let dir = MemoryManifestEntry::MemTree {
                base_manifest_id: Some(dir_nodehash.into_mercurial()),
                p1: Some(dir_nodehash.into_mercurial()),
                p2: None,
                changes: Arc::new(Mutex::new(BTreeMap::new())),
            };
            let path =
                MPathElement::new(b"dir".to_vec()).expect("dir is no longer a valid MPathElement");
            insert_entry(&mut memory_manifest.root_entry, path.clone(), dir);

            let manifest_id = memory_manifest
                .save()
                .wait()
                .expect("Could not save manifest");

            let refound = repo.get_manifest_by_nodeid(&manifest_id.get_hash().into_nodehash())
                .and_then(|m| m.lookup(&path))
                .wait()
                .expect("Lookup of entry just saved failed")
                .expect("Just saved entry not present");

            // Confirm that the entry we put in the root manifest is present
            assert_eq!(
                refound.get_hash().into_nodehash(),
                dir_nodehash,
                "directory hash changed"
            );
        })
    }

    #[test]
    fn remove_item() {
        async_unit::tokio_unit_test(|| {
            let repo = many_files_dirs::getrepo(None);
            let blobstore = repo.get_blobstore();
            let logger = Logger::root(Discard, o![]);

            let manifest_id = DNodeHash::from_static_str(
                "b267a6869fcc39b37741408b5823cc044233201d",
            ).expect("Could not get nodehash")
                .into_mercurial();

            let dir2 = MPathElement::new(b"dir2".to_vec()).expect("Can't create MPathElement dir2");

            // Load a memory manifest
            let memory_manifest =
                MemoryRootManifest::new(blobstore.clone(), logger, Some(&manifest_id), None)
                    .wait()
                    .expect("Could not load manifest");

            if !memory_manifest.root_entry.is_dir() {
                panic!("Loaded manifest is not a MemTree");
            }

            // Remove a file
            memory_manifest
                .change_entry(
                    &MPath::new(b"dir2/file_1_in_dir2").expect("Can't create MPath"),
                    None,
                )
                .wait()
                .expect("Failed to remove");

            // Assert that dir2 is now empty, since we've removed the item
            if let MemoryManifestEntry::MemTree { ref changes, .. } = memory_manifest.root_entry {
                let changes = changes.lock().expect("lock poisoned");
                assert!(
                    changes
                        .get(&dir2)
                        .expect("dir2 is missing")
                        .clone()
                        .map_or(false, |e| e.is_empty(&blobstore).wait().unwrap()),
                    "Bad after remove"
                );
                if let &Some(MemoryManifestEntry::MemTree { ref changes, .. }) =
                    changes.get(&dir2).expect("dir2 is missing")
                {
                    let changes = changes.lock().expect("lock poisoned");
                    assert!(!changes.is_empty(), "dir2 has no change entries");
                    assert!(
                        changes.values().all(Option::is_none),
                        "dir2 has some add entries"
                    );
                }
            } else {
                panic!("Loaded manifest is not a MemTree");
            }

            // And check that dir2 disappears over a save/reload operation
            let manifest_entry = memory_manifest
                .save()
                .wait()
                .expect("Could not save manifest");

            let refound = repo.get_manifest_by_nodeid(&manifest_entry.get_hash().into_nodehash())
                .and_then(|m| m.lookup(&dir2))
                .wait()
                .expect("Lookup of entry just saved failed");

            assert!(
                refound.is_none(),
                "Found dir2 when we should have deleted it on save"
            );
        })
    }

    #[test]
    fn add_item() {
        async_unit::tokio_unit_test(|| {
            let repo = many_files_dirs::getrepo(None);
            let blobstore = repo.get_blobstore();
            let logger = Logger::root(Discard, o![]);

            let manifest_id = DNodeHash::from_static_str(
                "b267a6869fcc39b37741408b5823cc044233201d",
            ).expect("Could not get nodehash")
                .into_mercurial();

            let new_file = MPathElement::new(b"new_file".to_vec())
                .expect("Can't create MPathElement new_file");

            // Load a memory manifest
            let memory_manifest =
                MemoryRootManifest::new(blobstore.clone(), logger, Some(&manifest_id), None)
                    .wait()
                    .expect("Could not load manifest");

            // Add a file
            let nodehash = DNodeHash::from_static_str("b267a6869fcc39b37741408b5823cc044233201d")
                .expect("Could not get nodehash");
            memory_manifest
                .change_entry(
                    &MPath::new(b"new_file").expect("Could not create MPath"),
                    Some(HgBlobEntry::new(
                        blobstore.clone(),
                        new_file.clone(),
                        nodehash,
                        Type::File(FileType::Regular),
                    )),
                )
                .wait()
                .expect("Failed to set");

            // And check that new_file persists
            let manifest_entry = memory_manifest
                .save()
                .wait()
                .expect("Could not save manifest");

            let refound = repo.get_manifest_by_nodeid(&manifest_entry.get_hash().into_nodehash())
                .and_then(|m| m.lookup(&new_file))
                .wait()
                .expect("Lookup of entry just saved failed")
                .expect("new_file did not persist");
            assert_eq!(
                refound.get_hash().into_nodehash(),
                nodehash,
                "nodehash hash changed"
            );
        })
    }

    #[test]
    fn replace_item() {
        async_unit::tokio_unit_test(|| {
            let repo = many_files_dirs::getrepo(None);
            let blobstore = repo.get_blobstore();
            let logger = Logger::root(Discard, o![]);

            let manifest_id = DNodeHash::from_static_str(
                "b267a6869fcc39b37741408b5823cc044233201d",
            ).expect("Could not get nodehash")
                .into_mercurial();

            let new_file = MPathElement::new(b"1".to_vec()).expect("Can't create MPathElement 1");

            // Load a memory manifest
            let memory_manifest =
                MemoryRootManifest::new(blobstore.clone(), logger, Some(&manifest_id), None)
                    .wait()
                    .expect("Could not load manifest");

            // Add a file
            let nodehash = DNodeHash::from_static_str("b267a6869fcc39b37741408b5823cc044233201d")
                .expect("Could not get nodehash");
            memory_manifest
                .change_entry(
                    &MPath::new(b"1").expect("Could not create MPath"),
                    Some(HgBlobEntry::new(
                        blobstore.clone(),
                        new_file.clone(),
                        nodehash,
                        Type::File(FileType::Regular),
                    )),
                )
                .wait()
                .expect("Failed to set");

            // And check that new_file persists
            let manifest_entry = memory_manifest
                .save()
                .wait()
                .expect("Could not save manifest");

            let refound = repo.get_manifest_by_nodeid(&manifest_entry.get_hash().into_nodehash())
                .and_then(|m| m.lookup(&new_file))
                .wait()
                .expect("Lookup of entry just saved failed")
                .expect("1 did not persist");
            assert_eq!(
                refound.get_hash().into_nodehash(),
                nodehash,
                "nodehash hash changed"
            );
        })
    }

    #[test]
    fn merge_manifests() {
        async_unit::tokio_unit_test(|| {
            let repo = many_files_dirs::getrepo(None);
            let blobstore = repo.get_blobstore();
            let logger = Logger::root(Discard, o![]);

            let base = {
                let mut changes = BTreeMap::new();
                let shared = MPathElement::new(b"shared".to_vec()).unwrap();
                let base = MPathElement::new(b"base".to_vec()).unwrap();
                let conflict = MPathElement::new(b"conflict".to_vec()).unwrap();
                changes.insert(
                    shared.clone(),
                    Some(MemoryManifestEntry::Blob(HgBlobEntry::new(
                        blobstore.clone(),
                        shared.clone(),
                        nodehash::ONES_HASH,
                        Type::File(FileType::Regular),
                    ))),
                );
                changes.insert(
                    base.clone(),
                    Some(MemoryManifestEntry::Blob(HgBlobEntry::new(
                        blobstore.clone(),
                        base.clone(),
                        nodehash::ONES_HASH,
                        Type::File(FileType::Regular),
                    ))),
                );
                changes.insert(
                    conflict.clone(),
                    Some(MemoryManifestEntry::Blob(HgBlobEntry::new(
                        blobstore.clone(),
                        conflict.clone(),
                        nodehash::ONES_HASH,
                        Type::File(FileType::Regular),
                    ))),
                );
                MemoryManifestEntry::MemTree {
                    base_manifest_id: None,
                    p1: Some(nodehash::ONES_HASH.into_mercurial()),
                    p2: None,
                    changes: Arc::new(Mutex::new(changes)),
                }
            };

            let other = {
                let mut changes = BTreeMap::new();
                let shared = MPathElement::new(b"shared".to_vec()).unwrap();
                let other = MPathElement::new(b"other".to_vec()).unwrap();
                let conflict = MPathElement::new(b"conflict".to_vec()).unwrap();
                changes.insert(
                    shared.clone(),
                    Some(MemoryManifestEntry::Blob(HgBlobEntry::new(
                        blobstore.clone(),
                        shared.clone(),
                        nodehash::ONES_HASH,
                        Type::File(FileType::Regular),
                    ))),
                );
                changes.insert(
                    other.clone(),
                    Some(MemoryManifestEntry::Blob(HgBlobEntry::new(
                        blobstore.clone(),
                        other.clone(),
                        nodehash::TWOS_HASH,
                        Type::File(FileType::Regular),
                    ))),
                );
                changes.insert(
                    conflict.clone(),
                    Some(MemoryManifestEntry::Blob(HgBlobEntry::new(
                        blobstore.clone(),
                        conflict.clone(),
                        nodehash::TWOS_HASH,
                        Type::File(FileType::Regular),
                    ))),
                );
                MemoryManifestEntry::MemTree {
                    base_manifest_id: None,
                    p1: Some(nodehash::TWOS_HASH.into_mercurial()),
                    p2: None,
                    changes: Arc::new(Mutex::new(changes)),
                }
            };

            let merged = base.merge_with_conflicts(other, blobstore, logger, RepoPath::root())
                .wait()
                .unwrap();

            if let MemoryManifestEntry::MemTree { changes, .. } = merged {
                let changes = changes.lock().expect("lock poisoned");
                assert_eq!(changes.len(), 4, "Should merge to 4 entries");
                if let Some(&Some(MemoryManifestEntry::Blob(ref blob))) =
                    changes.get(&MPathElement::new(b"shared".to_vec()).unwrap())
                {
                    assert_eq!(
                        blob.get_hash(),
                        &DEntryId::new(nodehash::ONES_HASH),
                        "Wrong hash for shared"
                    );
                } else {
                    panic!("shared is not a blob");
                }
                if let Some(&Some(MemoryManifestEntry::Blob(ref blob))) =
                    changes.get(&MPathElement::new(b"base".to_vec()).unwrap())
                {
                    assert_eq!(
                        blob.get_hash(),
                        &DEntryId::new(nodehash::ONES_HASH),
                        "Wrong hash for base"
                    );
                } else {
                    panic!("base is not a blob");
                }
                if let Some(&Some(MemoryManifestEntry::Blob(ref blob))) =
                    changes.get(&MPathElement::new(b"other".to_vec()).unwrap())
                {
                    assert_eq!(
                        blob.get_hash(),
                        &DEntryId::new(nodehash::TWOS_HASH),
                        "Wrong hash for other"
                    );
                } else {
                    panic!("other is not a blob");
                }
                if let Some(&Some(MemoryManifestEntry::Conflict(ref conflicts))) =
                    changes.get(&MPathElement::new(b"conflict".to_vec()).unwrap())
                {
                    assert_eq!(conflicts.len(), 2, "Should have two conflicts");
                } else {
                    panic!("conflict did not create a conflict")
                }
            } else {
                panic!("Merge failed to produce a merged tree");
            }
        })
    }
}