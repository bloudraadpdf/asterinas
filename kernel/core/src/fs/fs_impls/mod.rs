// SPDX-License-Identifier: MPL-2.0

//! Concrete file system implementations.
//!
//! This module contains all the specific file system implementations supported by the kernel.

pub(crate) mod cgroupfs;
#[cfg(feature = "configfs")]
pub(crate) mod configfs;
pub(crate) mod devpts;
pub mod devtmpfs;
#[cfg(feature = "filesystems_extra")]
pub(crate) mod exfat;
#[cfg(feature = "filesystems_extra")]
pub(crate) mod ext2;
#[cfg(feature = "filesystems_extra")]
pub(crate) mod overlayfs;
pub(crate) mod procfs;
pub(crate) mod pseudofs;
pub(crate) mod ramfs;
pub(crate) mod sysfs;
pub(crate) mod tmpfs;
#[cfg(feature = "filesystems_extra")]
pub(crate) mod virtiofs;

pub(super) fn init() {
    sysfs::init();
    procfs::init();
    cgroupfs::init();
    #[cfg(feature = "configfs")]
    configfs::init();
    ramfs::init();
    tmpfs::init();
    devtmpfs::init();
    devpts::init();
    pseudofs::init();

    #[cfg(feature = "filesystems_extra")]
    ext2::init();
    #[cfg(feature = "filesystems_extra")]
    exfat::init();
    #[cfg(feature = "filesystems_extra")]
    overlayfs::init();
    #[cfg(feature = "filesystems_extra")]
    virtiofs::init();
}

pub(super) fn init_on_each_cpu() {
    procfs::init_on_each_cpu();
}

pub(super) fn init_in_first_kthread() {
    devtmpfs::init_in_first_kthread();
}
