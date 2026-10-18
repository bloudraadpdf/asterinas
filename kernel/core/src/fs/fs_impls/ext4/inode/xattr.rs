// SPDX-License-Identifier: MPL-2.0

//! Ext4 attribute namespace translation and bounded user copies.

use aster_ext4::AttributeMode;

use super::{Ext4Inode, map_error, timestamp};
use crate::{
    fs::vfs::xattr::{
        XATTR_LIST_MAX_LEN, XATTR_VALUE_MAX_LEN, XattrName, XattrNamespace, XattrSetFlags,
    },
    prelude::*,
};

impl Ext4Inode {
    fn attributes(&self) -> Result<Vec<aster_ext4::Attribute>> {
        let volume = self.volume().lock();
        let reader = volume.reader()?;
        let raw = reader.inode(self.number).map_err(map_error)?;
        reader.attributes(&raw).map_err(map_error)
    }

    pub(super) fn get_attribute(&self, name: XattrName, writer: &mut VmWriter) -> Result<usize> {
        let (index, suffix) = disk_name(name)?;
        let attributes = self.attributes()?;
        let value = attributes
            .iter()
            .find(|attribute| attribute.index() == index && attribute.name() == suffix)
            .ok_or_else(|| Error::new(Errno::ENODATA))?
            .value();
        copy_out(value, writer)
    }

    pub(super) fn list_attributes(
        &self,
        namespace: XattrNamespace,
        writer: &mut VmWriter,
    ) -> Result<usize> {
        let attributes = self.attributes()?;
        let mut names = Vec::new();
        for attribute in attributes {
            if namespace == XattrNamespace::User && attribute.index() != 1 {
                continue;
            }
            let prefix = match attribute.index() {
                1 => b"user.".as_slice(),
                2 => b"system.posix_acl_access".as_slice(),
                3 => b"system.posix_acl_default".as_slice(),
                4 => b"trusted.".as_slice(),
                6 => b"security.".as_slice(),
                7 => b"system.".as_slice(),
                _ => continue,
            };
            if names.len() + prefix.len() + attribute.name().len() + 1 > XATTR_LIST_MAX_LEN {
                return_errno!(Errno::E2BIG);
            }
            names.extend_from_slice(prefix);
            names.extend_from_slice(attribute.name());
            names.push(0);
        }
        copy_out(&names, writer)
    }

    pub(super) fn set_attribute(
        &self,
        name: XattrName,
        reader: &mut VmReader,
        flags: XattrSetFlags,
    ) -> Result<()> {
        let (index, suffix) = disk_name(name)?;
        if matches!(index, 2 | 3) {
            return_errno!(Errno::EOPNOTSUPP);
        }
        if reader.remain() > XATTR_VALUE_MAX_LEN {
            return_errno!(Errno::E2BIG);
        }
        let mut bytes = vec![0; reader.remain()];
        VmWriter::from(bytes.as_mut_slice()).write_fallible(reader)?;
        let mode = match flags {
            XattrSetFlags::CREATE_OR_REPLACE => AttributeMode::Upsert,
            XattrSetFlags::CREATE_ONLY => AttributeMode::Create,
            XattrSetFlags::REPLACE_ONLY => AttributeMode::Replace,
            _ => return_errno!(Errno::EINVAL),
        };
        let changed = timestamp(Self::now())?;
        self.volume().lock().mutate(&[self.number], |writer| {
            writer.set_attribute(self.number, index, suffix, &bytes, mode, changed)
        })
    }

    pub(super) fn remove_attribute(&self, name: XattrName) -> Result<()> {
        let (index, suffix) = disk_name(name)?;
        if matches!(index, 2 | 3) {
            return_errno!(Errno::EOPNOTSUPP);
        }
        let changed = timestamp(Self::now())?;
        self.volume().lock().mutate(&[self.number], |writer| {
            writer.remove_attribute(self.number, index, suffix, changed)
        })
    }
}

fn disk_name(name: XattrName<'_>) -> Result<(u8, &[u8])> {
    match name.full_name() {
        "system.posix_acl_access" => Ok((2, &[])),
        "system.posix_acl_default" => Ok((3, &[])),
        full => {
            let index = match name.namespace() {
                XattrNamespace::User => 1,
                XattrNamespace::Trusted => 4,
                XattrNamespace::Security => 6,
                XattrNamespace::System => 7,
            };
            let (_, suffix) = full
                .split_once('.')
                .ok_or_else(|| Error::new(Errno::EINVAL))?;
            Ok((index, suffix.as_bytes()))
        }
    }
}

fn copy_out(bytes: &[u8], writer: &mut VmWriter) -> Result<usize> {
    if writer.avail() == 0 {
        return Ok(bytes.len());
    }
    if writer.avail() < bytes.len() {
        return_errno!(Errno::ERANGE);
    }
    writer.write_fallible(&mut VmReader::from(bytes))?;
    Ok(bytes.len())
}
