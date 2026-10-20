// SPDX-License-Identifier: MPL-2.0

use ostd::task::disable_preempt;

use super::Vmar;
use crate::{
    fs::file::Permission,
    prelude::*,
    process::{UserNamespace, credentials::capabilities::CapSet},
    security::lsm::hooks as lsm_hooks,
};

impl Vmar {
    /// Queries residency without populating pages, stopping at the first mapping boundary.
    pub(crate) fn query_residency(
        &self,
        address: Vaddr,
        states: &mut [u8],
        ctx: &Context,
    ) -> Result<usize> {
        let inner = self.inner.read();
        let mapping = inner
            .vm_mappings
            .find_one(&address)
            .ok_or_else(|| Error::new(Errno::ENOMEM))?;
        let count = states.len().min((mapping.map_end() - address) / PAGE_SIZE);
        debug_assert!(count > 0);
        if let Some(inode) = mapping.inode() {
            let credentials = ctx.posix_thread.credentials();
            let may_query = inode.owner().is_ok_and(|uid| uid == credentials.fsuid())
                || inode.check_permission(Permission::MAY_WRITE).is_ok()
                || lsm_hooks::on_capable(lsm_hooks::CapableContext::new(
                    UserNamespace::get_init_singleton().as_ref(),
                    ctx.posix_thread,
                    CapSet::FOWNER,
                ))
                .is_ok();
            // Linux masks file-cache residency when the caller cannot own or write the file.
            if !may_query {
                states[..count].fill(1);
                return Ok(count);
            }
        }
        let range = address..address + count * PAGE_SIZE;
        let preempt_guard = disable_preempt();
        let mut cursor = self.vm_space.cursor(&preempt_guard, &range).unwrap();
        for (index, state) in states[..count].iter_mut().enumerate() {
            let page_address = address + index * PAGE_SIZE;
            cursor.jump(page_address).unwrap();
            let present = cursor.query().unwrap().1.is_some();
            let cached = !present
                && mapping.vmo().is_some_and(|mapped| {
                    mapped
                        .offset()
                        .checked_add(page_address - mapping.map_to_addr())
                        .is_some_and(|offset| mapped.vmo().is_page_resident(offset))
                });
            *state = u8::from(present || cached);
        }
        Ok(count)
    }
}
