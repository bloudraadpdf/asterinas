// SPDX-License-Identifier: MPL-2.0

//! Reclamation preserves live paths and releases cached parent-child cycles.

use core::time::Duration;

use ostd::prelude::ktest;

use super::{Dentry, DentryOptions};
use crate::{
    fs::{
        file::{InodeMode, InodeType, StatusFlags},
        vfs::{
            file_system::FileSystem,
            inode::{Extension, FileOps, Inode, Metadata},
        },
    },
    prelude::*,
    process::{Gid, Uid},
};

struct CacheTestInode {
    type_: InodeType,
    extension: Extension,
}

impl CacheTestInode {
    fn unused<T>() -> T {
        panic!("cache reclamation accessed an inode operation");
    }
}

impl FileOps for CacheTestInode {
    fn read_at(
        &self,
        _offset: usize,
        _writer: &mut VmWriter,
        _flags: StatusFlags,
    ) -> Result<usize> {
        Self::unused()
    }
    fn write_at(
        &self,
        _offset: usize,
        _reader: &mut VmReader,
        _flags: StatusFlags,
    ) -> Result<usize> {
        Self::unused()
    }
}

impl Inode for CacheTestInode {
    fn type_(&self) -> InodeType {
        self.type_
    }
    fn extension(&self) -> &Extension {
        &self.extension
    }
    fn size(&self) -> usize {
        Self::unused()
    }
    fn ino(&self) -> u64 {
        Self::unused()
    }
    fn metadata(&self) -> Result<Metadata> {
        Self::unused()
    }
    fn resize(&self, _dentry: &Dentry, _size: usize) -> Result<()> {
        Self::unused()
    }
    fn mode(&self) -> Result<InodeMode> {
        Self::unused()
    }
    fn set_mode(&self, _dentry: &Dentry, _mode: InodeMode) -> Result<()> {
        Self::unused()
    }
    fn owner(&self) -> Result<Uid> {
        Self::unused()
    }
    fn set_owner(&self, _dentry: &Dentry, _uid: Uid) -> Result<()> {
        Self::unused()
    }
    fn group(&self) -> Result<Gid> {
        Self::unused()
    }
    fn set_group(&self, _dentry: &Dentry, _gid: Gid) -> Result<()> {
        Self::unused()
    }
    fn atime(&self) -> Duration {
        Self::unused()
    }
    fn set_atime(&self, _dentry: &Dentry, _time: Duration) {
        Self::unused::<()>()
    }
    fn mtime(&self) -> Duration {
        Self::unused()
    }
    fn set_mtime(&self, _dentry: &Dentry, _time: Duration) {
        Self::unused::<()>()
    }
    fn ctime(&self) -> Duration {
        Self::unused()
    }
    fn set_ctime(&self, _dentry: &Dentry, _time: Duration) {
        Self::unused::<()>()
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Self::unused()
    }
}

fn inode(type_: InodeType) -> Arc<dyn Inode> {
    Arc::new(CacheTestInode {
        type_,
        extension: Extension::new(),
    })
}

fn child(parent: &Arc<Dentry>, name: &str, type_: InodeType) -> Arc<Dentry> {
    let child = Dentry::new(
        inode(type_),
        DentryOptions::Named((name.to_string(), parent.clone())),
    );
    parent
        .dir_state
        .as_ref()
        .unwrap()
        .children
        .write()
        .insert_positive(name.to_string(), child.clone());
    child
}

fn cached_parent_child_cycles_are_reclaimed() {
    let root = Dentry::new_root(inode(InodeType::Dir));
    let directory = child(&root, "directory", InodeType::Dir);
    let file = child(&directory, "file", InodeType::File);
    let weak_directory = Arc::downgrade(&directory);
    let weak_file = Arc::downgrade(&file);
    let weak_root = Arc::downgrade(&root);
    drop(file);
    drop(directory);
    assert!(weak_directory.upgrade().is_some());
    root.prune_unused_children();
    assert!(weak_directory.upgrade().is_none());
    assert!(weak_file.upgrade().is_none());
    drop(root);
    assert!(weak_root.upgrade().is_none());
}

fn cache_reclamation_preserves_live_paths() {
    let root = Dentry::new_root(inode(InodeType::Dir));
    let directory = child(&root, "directory", InodeType::Dir);
    let file = child(&directory, "file", InodeType::File);
    let unused = child(&root, "unused", InodeType::File);
    let weak_directory = Arc::downgrade(&directory);
    let weak_unused = Arc::downgrade(&unused);
    drop(unused);
    drop(directory);
    root.prune_unused_children();
    assert!(weak_unused.upgrade().is_none());
    let directory = weak_directory.upgrade().unwrap();
    let cached = directory
        .dir_state
        .as_ref()
        .unwrap()
        .children
        .read()
        .find("file")
        .unwrap()
        .into_dentry()
        .unwrap();
    assert!(Arc::ptr_eq(&file, &cached));
    drop(cached);
    drop(directory);
    drop(file);
    root.prune_unused_children();
    assert!(weak_directory.upgrade().is_none());
}

fn deep_cache_reclamation_is_iterative() {
    let root = Dentry::new_root(inode(InodeType::Dir));
    let mut directory = root.clone();
    for _ in 0..4096 {
        directory = child(&directory, "nested", InodeType::Dir);
    }
    let deepest = Arc::downgrade(&directory);
    drop(directory);
    root.prune_unused_children();
    assert!(deepest.upgrade().is_none());
    assert!(
        root.dir_state
            .as_ref()
            .unwrap()
            .children
            .read()
            .entries
            .is_empty()
    );
}

#[ktest]
fn cache_reclamation_regressions() {
    cached_parent_child_cycles_are_reclaimed();
    cache_reclamation_preserves_live_paths();
    deep_cache_reclamation_is_iterative();
}
