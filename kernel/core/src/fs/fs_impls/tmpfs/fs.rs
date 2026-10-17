// SPDX-License-Identifier: MPL-2.0

use super::options::MountOptions;
use crate::{
    fs::{
        fs_impls::ramfs::{self, RamFs, RamFsConfig, RamFsQuota},
        pseudofs::AnonDeviceId,
        vfs::{
            file_system::{FileSystem, SuperBlock},
            inode::RevalidationPolicy,
            registry::{FsCreationCtx, FsProperties, FsType},
        },
    },
    prelude::*,
};

const TMPFS_MAGIC: u64 = 0x0102_1994;

/// RAM-backed temporary files, with per-mount page and inode limits.
pub(crate) type TmpFs = RamFs;

impl TmpFs {
    /// Creates a tmpfs filesystem instance.
    pub(in crate::fs) fn new_tmpfs() -> Arc<Self> {
        Self::new_tmpfs_backing("tmpfs", RevalidationPolicy::empty())
    }

    /// Creates a tmpfs-backed filesystem instance with a custom filesystem name
    /// and directory-entry revalidation policy.
    pub(in crate::fs) fn new_tmpfs_backing(
        name: &'static str,
        revalidation_policy: RevalidationPolicy,
    ) -> Arc<Self> {
        Self::new_tmpfs_instance(
            name,
            revalidation_policy,
            RamFsConfig::default(),
            default_max_blocks(),
            default_max_inodes(),
        )
    }

    fn new_tmpfs_instance(
        name: &'static str,
        revalidation_policy: RevalidationPolicy,
        config: RamFsConfig,
        max_blocks: usize,
        max_inodes: usize,
    ) -> Arc<Self> {
        let anon_device_id = AnonDeviceId::acquire().expect("no device ID is available for tmpfs");
        let sb = {
            let mut super_block = SuperBlock::new(
                TMPFS_MAGIC,
                ramfs::BLOCK_SIZE,
                ramfs::NAME_MAX,
                anon_device_id.id(),
            );
            super_block.blocks = max_blocks;
            super_block.bfree = max_blocks;
            super_block.bavail = max_blocks;
            super_block.files = max_inodes;
            super_block.ffree = max_inodes;
            super_block
        };
        Self::new_with_sb(name, anon_device_id, sb, revalidation_policy, config)
    }
}

fn default_max_blocks() -> usize {
    crate::vm::mem_total() / PAGE_SIZE / 2
}
fn default_max_inodes() -> usize {
    crate::vm::mem_total() / PAGE_SIZE / 2
}

pub(super) struct TmpFsType;

impl FsType for TmpFsType {
    type Key = ();

    fn name(&self) -> &'static str {
        "tmpfs"
    }

    fn properties(&self) -> FsProperties {
        FsProperties::empty()
    }

    fn create(&self, ctx: &mut FsCreationCtx) -> Result<Arc<dyn FileSystem>> {
        let options = MountOptions::parse(ctx.args())?;
        let config = RamFsConfig {
            mode: options.mode,
            uid: options.uid,
            gid: options.gid,
            quota: Some(RamFsQuota::new(options.pages, options.inodes)?),
        };
        Ok(TmpFs::new_tmpfs_instance(
            "tmpfs",
            RevalidationPolicy::empty(),
            config,
            options.pages,
            options.inodes,
        ))
    }

    fn sysnode(&self) -> Option<Arc<dyn aster_systree::SysNode>> {
        None
    }
}
