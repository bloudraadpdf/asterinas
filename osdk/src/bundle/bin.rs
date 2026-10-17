// SPDX-License-Identifier: MPL-2.0

use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::SystemTime,
};

use super::file::BundleFile;
use crate::{
    arch::Arch,
    config::scheme::{BootMethod, BootProtocol},
    util::hard_link_or_copy,
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AsterBin {
    path: PathBuf,
    arch: Arch,
    typ: AsterBinType,
    version: String,
    modified_time: SystemTime,
    size: u64,
    stripped: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AsterBinType {
    Elf(AsterElfMeta),
    BzImage(AsterBzImageMeta),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AsterElfMeta {
    pub has_linux_header: bool,
    pub has_pvh_header: bool,
    pub has_multiboot_header: bool,
    pub has_multiboot2_header: bool,
}

impl AsterElfMeta {
    pub fn read(path: &Path) -> Self {
        let bytes = fs::read(path).expect("cannot read kernel ELF");
        let elf = xmas_elf::ElfFile::new(&bytes).expect("invalid kernel ELF");
        let has_section = |name| {
            elf.find_section_by_name(name)
                .is_some_and(|section| section.size() != 0)
        };
        Self {
            has_linux_header: false,
            has_pvh_header: has_section(".note.Xen"),
            has_multiboot_header: has_section(".multiboot_header"),
            has_multiboot2_header: has_section(".multiboot2_header"),
        }
    }

    pub fn is_pvh_only(&self) -> bool {
        self.has_pvh_header && !self.has_multiboot_header && !self.has_multiboot2_header
    }

    pub fn validate_boot(
        &self,
        method: BootMethod,
        protocol: BootProtocol,
        linux_legacy: bool,
    ) -> Result<(), &'static str> {
        if self.is_pvh_only()
            && (method != BootMethod::VmmDirect || protocol == BootProtocol::Linux || linux_legacy)
        {
            return Err("PVH-only kernels require vmm-direct and cannot use Linux or GRUB boot");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AsterBzImageMeta {
    pub support_legacy32_boot: bool,
    pub support_efi_boot: bool,
    pub support_efi_handover: bool,
}

impl BundleFile for AsterBin {
    fn path(&self) -> &PathBuf {
        &self.path
    }

    fn modified_time(&self) -> &SystemTime {
        &self.modified_time
    }

    fn size(&self) -> &u64 {
        &self.size
    }
}

impl AsterBin {
    pub fn new(
        path: impl AsRef<Path>,
        arch: Arch,
        typ: AsterBinType,
        version: String,
        stripped: bool,
    ) -> Self {
        let created = Self {
            path: path.as_ref().to_path_buf(),
            arch,
            typ,
            version,
            modified_time: SystemTime::UNIX_EPOCH,
            size: 0,
            stripped,
        };
        Self {
            modified_time: created.get_modified_time(),
            size: created.get_size(),
            ..created
        }
    }

    pub fn arch(&self) -> Arch {
        self.arch
    }

    pub fn version(&self) -> &String {
        &self.version
    }

    pub fn stripped(&self) -> bool {
        self.stripped
    }

    pub fn typ(&self) -> &AsterBinType {
        &self.typ
    }

    /// Copy the binary to the `base` directory and convert the path to a relative path.
    pub fn copy_to(self, base: impl AsRef<Path>) -> Self {
        let file_name = self.path.file_name().unwrap();
        let copied_path = base.as_ref().join(file_name);
        hard_link_or_copy(&self.path, &copied_path).unwrap();
        let copied_metadata = copied_path.metadata().unwrap();
        Self {
            path: PathBuf::from(file_name),
            arch: self.arch,
            typ: self.typ,
            version: self.version,
            modified_time: copied_metadata.modified().unwrap(),
            size: copied_metadata.size(),
            stripped: self.stripped,
        }
    }
}
