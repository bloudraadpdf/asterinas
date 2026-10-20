// SPDX-License-Identifier: MPL-2.0

use crate::prelude::*;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AnonVmaName(Arc<str>);

impl AnonVmaName {
    pub(crate) const MAX_BYTES: usize = 80;

    pub(crate) fn new(name: &CStr) -> Result<Self> {
        let bytes = name.to_bytes();
        if bytes.len() >= Self::MAX_BYTES
            || bytes
                .iter()
                .any(|byte| !(0x20..0x7f).contains(byte) || b"\\`$[]".contains(byte))
        {
            return_errno!(Errno::EINVAL);
        }
        let name = name.to_str().map_err(|_| Error::new(Errno::EINVAL))?;
        Ok(Self(Arc::from(name)))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}
