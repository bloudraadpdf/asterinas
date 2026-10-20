// SPDX-License-Identifier: MPL-2.0

use core::ops::Range;

use super::{Interval, Vmar, util::get_intersected_range};
use crate::{prelude::*, vm::vmar::AnonVmaName};

impl Vmar {
    /// Names anonymous mappings, retaining changes around unmapped holes.
    pub(crate) fn name_anonymous(
        &self,
        range: Range<Vaddr>,
        name: Option<AnonVmaName>,
    ) -> Result<()> {
        let mut inner = self.inner.write();
        let mut position = range.start;
        let mut has_hole = false;
        while position < range.end {
            let Some((mapping_range, can_name, is_unchanged)) = inner
                .vm_mappings
                .find(&(position..range.end))
                .next()
                .map(|mapping| {
                    (
                        mapping.range(),
                        mapping.can_name_anonymous(),
                        mapping.anon_name() == name.as_ref(),
                    )
                })
            else {
                has_hole = true;
                break;
            };
            if position < mapping_range.start {
                has_hole = true;
            }
            if !can_name {
                return_errno!(Errno::EBADF);
            }
            let selected = get_intersected_range(&(position..range.end), &mapping_range);
            position = selected.end;
            if is_unchanged {
                continue;
            }
            let (mapping, mut rmap_to_remove) = inner.remove(&mapping_range.start).unwrap();
            let mut rmap = rmap_to_remove.remove(self, mapping_range.start);
            let mut selected =
                inner.extract_mapping_range(self, mapping, &selected, rmap.as_deref_mut());
            selected.set_anon_name(name.clone());
            inner.insert_try_merge(self, selected, rmap.as_deref_mut());
        }
        if has_hole {
            return_errno!(Errno::ENOMEM);
        }
        Ok(())
    }
}
