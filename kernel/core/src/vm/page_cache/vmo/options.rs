// SPDX-License-Identifier: MPL-2.0

//! Options for allocating root and child VMOs.

use core::sync::atomic::AtomicUsize;

use align_ext::AlignExt;
use ostd::mm::{AllocationPriority, FrameAllocOptions, Segment};
use xarray::XArray;

use super::{Vmo, VmoFlags, WritableMappingStatus};
use crate::{
    prelude::*,
    util::quota::Quota,
    vm::{
        page_cache::{CachePage, CachePageMeta, PageCacheBackend},
        vmar::Rmap,
    },
};

/// Options for allocating a root VMO.
///
/// # Examples
///
/// Creating a generic VMO:
/// ```
/// use ostd::mm::PAGE_SIZE;
///
/// use crate::vm::page_cache::VmoOptions;
///
/// let vmo = VmoOptions::new(PAGE_SIZE).alloc();
/// assert!(vmo.is_ok());
/// ```
///
/// Creating an anonymous page-cache-style VMO:
/// ```
/// use ostd::mm::PAGE_SIZE;
///
/// use crate::vm::page_cache::VmoOptions;
///
/// let vmo = VmoOptions::new_anon(10 * PAGE_SIZE).alloc();
/// assert!(vmo.is_ok());
/// ```
pub(crate) struct VmoOptions {
    size: usize,
    flags: VmoFlags,
    quota: Option<Arc<Quota>>,
    backend: Option<Weak<dyn PageCacheBackend>>,
}

impl VmoOptions {
    /// Creates a default set of options with the specified size of the VMO
    /// (in bytes).
    ///
    /// The size of the VMO will be rounded up to align with the page size.
    ///
    /// This is the low-level builder entry point. Call [`VmoOptions::new_anon`]
    /// for resizable RAM-backed page-cache-style VMOs, or
    /// [`VmoOptions::new_page_cache`] for page-cache VMOs with a backend.
    pub(crate) fn new(size: usize) -> Self {
        Self {
            size,
            flags: VmoFlags::empty(),
            quota: None,
            backend: None,
        }
    }

    /// Creates options for an anonymous resizable VMO.
    pub(crate) fn new_anon(size: usize) -> Self {
        Self::new(size).flags(VmoFlags::RESIZABLE)
    }

    /// Creates options for a page-cache VMO with a backend.
    #[cfg(any(
        feature = "filesystems_extra",
        feature = "fs_erofs",
        feature = "fs_ext4",
        ktest
    ))]
    pub(crate) fn new_page_cache(size: usize, backend: Weak<dyn PageCacheBackend>) -> Self {
        Self::new(size).flags(VmoFlags::RESIZABLE).backend(backend)
    }

    pub(crate) fn quota(mut self, quota: Option<Arc<Quota>>) -> Self {
        self.quota = quota;
        self
    }

    /// Sets the VMO flags.
    ///
    /// The default value is `VmoFlags::empty()`.
    ///
    /// For more information about the flags, see `VmoFlags`.
    pub(crate) fn flags(mut self, flags: VmoFlags) -> Self {
        self.flags = flags;
        self
    }

    /// Sets the backend for the VMO.
    #[cfg(any(
        feature = "filesystems_extra",
        feature = "fs_erofs",
        feature = "fs_ext4",
        ktest
    ))]
    fn backend(mut self, backend: Weak<dyn PageCacheBackend>) -> Self {
        self.backend = Some(backend);
        self
    }
}

impl VmoOptions {
    /// Allocates the VMO according to the specified options.
    pub(crate) fn alloc(self) -> Result<Arc<Vmo>> {
        let VmoOptions {
            size,
            flags,
            backend,
            quota,
            ..
        } = self;
        let vmo = alloc_vmo(size, flags, backend, quota)?;
        Ok(Arc::new(vmo))
    }
}

fn alloc_vmo(
    size: usize,
    flags: VmoFlags,
    backend: Option<Weak<dyn PageCacheBackend>>,
    quota: Option<Arc<Quota>>,
) -> Result<Vmo> {
    let size = size.align_up(PAGE_SIZE);
    let pages = committed_pages_if_continuous(flags, size)?;
    let committed_pages = if flags.contains(VmoFlags::CONTIGUOUS) {
        size / PAGE_SIZE
    } else {
        0
    };
    let writable_mapping_status = WritableMappingStatus::default();
    let rmap = Rmap::new();
    Ok(Vmo {
        backend,
        flags,
        quota,
        pages,
        committed_pages: AtomicUsize::new(committed_pages),
        size: AtomicUsize::new(size),
        writable_mapping_status,
        rmap: Mutex::new(rmap),
    })
}

fn committed_pages_if_continuous(flags: VmoFlags, size: usize) -> Result<XArray<CachePage>> {
    if flags.contains(VmoFlags::CONTIGUOUS) {
        // if the vmo is continuous, we need to allocate frames for the vmo
        let frames_num = size / PAGE_SIZE;
        let segment: Segment<CachePageMeta> = FrameAllocOptions::new()
            .priority(AllocationPriority::Normal)
            .alloc_segment_with(frames_num, |_| CachePageMeta::default())?;
        let committed_pages = XArray::new();
        let mut locked_pages = committed_pages.lock();
        let mut cursor = locked_pages.cursor_mut(0);
        for frame in segment {
            cursor.store(frame);
            cursor.next();
        }
        drop(locked_pages);
        Ok(committed_pages)
    } else {
        // otherwise, we wait for the page is read or write
        Ok(XArray::new())
    }
}

#[cfg(ktest)]
mod test {
    use ostd::{mm::VmIo, prelude::*};

    use super::*;

    #[ktest]
    fn alloc_vmo() {
        let vmo = VmoOptions::new(PAGE_SIZE).alloc().unwrap();
        assert_eq!(vmo.size(), PAGE_SIZE);
        assert_eq!(vmo.committed_pages(), 0);
        assert!(!vmo.is_page_resident(0));
        assert_eq!(vmo.committed_pages(), 0);
        // the vmo is zeroed once allocated
        let mut read_buffer = [0; size_of::<usize>()];
        let mut writer = VmWriter::from(read_buffer.as_mut_slice()).to_fallible();
        vmo.read(0, &mut writer).unwrap();
        assert_eq!(usize::from_ne_bytes(read_buffer), 0);
        assert_eq!(vmo.committed_pages(), 1);
        assert!(vmo.is_page_resident(0));
        assert!(!vmo.is_page_resident(PAGE_SIZE));
    }

    #[ktest]
    fn alloc_continuous_vmo() {
        let vmo = VmoOptions::new(10 * PAGE_SIZE)
            .flags(VmoFlags::CONTIGUOUS)
            .alloc()
            .unwrap();
        assert_eq!(vmo.size(), 10 * PAGE_SIZE);
        assert_eq!(vmo.committed_pages(), 10);
    }

    #[ktest]
    fn write_and_read() {
        let vmo = VmoOptions::new(PAGE_SIZE).alloc().unwrap();
        let val = 42u8;
        // write val
        let mut reader = VmReader::from(core::slice::from_ref(&val)).to_fallible();
        vmo.write(111, &mut reader).unwrap();

        let mut read_buffer = [0];
        let mut writer = VmWriter::from(read_buffer.as_mut_slice()).to_fallible();
        vmo.read(111, &mut writer).unwrap();
        let read_val = read_buffer[0];
        assert_eq!(val, read_val);
        // bit endian
        let mut reader = VmReader::from(&[0x12, 0x34, 0x56, 0x78][..]).to_fallible();
        vmo.write(222, &mut reader).unwrap();

        let mut read_buffer = [0; size_of::<u32>()];
        let mut writer = VmWriter::from(read_buffer.as_mut_slice()).to_fallible();
        vmo.read(222, &mut writer).unwrap();
        let read_val = u32::from_ne_bytes(read_buffer);
        assert_eq!(read_val, 0x78563412)
    }

    #[ktest]
    fn resize() {
        use crate::vm::page_cache::PageCache;

        let mut page_cache = PageCache::new_anon(PAGE_SIZE, None).unwrap();
        page_cache.write_val(10, &42u8).unwrap();
        page_cache.resize(2 * PAGE_SIZE, page_cache.size()).unwrap();
        assert_eq!(page_cache.size(), 2 * PAGE_SIZE);
        assert_eq!(page_cache.read_val::<u8>(10).unwrap(), 42);
        page_cache.write_val(PAGE_SIZE + 20, &123u8).unwrap();
        assert_eq!(page_cache.as_vmo().committed_pages(), 2);
        page_cache.resize(PAGE_SIZE, page_cache.size()).unwrap();
        assert_eq!(page_cache.as_vmo().committed_pages(), 1);
        assert_eq!(page_cache.read_val::<u8>(10).unwrap(), 42);
    }

    #[ktest]
    fn preallocation_preserves_eof_and_same_size_truncate_releases_it() {
        use crate::vm::page_cache::{PageCache, VmoCommitError, VmoMapMode};

        let mut cache = PageCache::new_anon(PAGE_SIZE, None).unwrap();
        cache.preallocate(2 * PAGE_SIZE..4 * PAGE_SIZE).unwrap();
        assert_eq!(cache.as_vmo().size(), PAGE_SIZE);
        assert_eq!(cache.as_vmo().committed_pages(), 2);
        assert!(!cache.as_vmo().is_page_resident(2 * PAGE_SIZE));
        assert!(matches!(
            cache
                .as_vmo()
                .try_commit_page(2 * PAGE_SIZE, VmoMapMode::SharedRead),
            Err(VmoCommitError::Err(error)) if error.error() == Errno::ENXIO
        ));
        assert_eq!(
            cache
                .as_vmo()
                .commit_on(2, VmoMapMode::SharedRead)
                .unwrap_err()
                .error(),
            Errno::ENXIO
        );
        assert_eq!(cache.as_vmo().committed_pages(), 2);
        cache.resize(PAGE_SIZE, PAGE_SIZE).unwrap();
        assert_eq!(cache.as_vmo().committed_pages(), 0);
    }

    #[ktest]
    fn failed_preallocation_releases_only_new_pages() {
        use crate::vm::page_cache::PageCache;

        let quota = Quota::new(2);
        let mut cache = PageCache::new_anon(PAGE_SIZE, Some(quota.clone())).unwrap();
        cache.write_val(0, &42u8).unwrap();
        let error = cache.preallocate(0..3 * PAGE_SIZE).unwrap_err();
        assert_eq!(error.error(), Errno::ENOSPC);
        assert_eq!(cache.as_vmo().committed_pages(), 1);
        assert_eq!(quota.remaining(), 1);
        assert_eq!(cache.read_val::<u8>(0).unwrap(), 42);
    }
    #[ktest]
    fn resident_page_walk_does_not_allocate_sparse_neighbors() {
        let vmo = VmoOptions::new_anon(4 * PAGE_SIZE)
            .quota(Some(Quota::new(1)))
            .alloc()
            .unwrap();
        vmo.commit_on(1, super::super::VmoMapMode::SharedRead)
            .unwrap();
        let mut visited = Vec::new();
        vmo.for_each_resident_page(
            &(0..4 * PAGE_SIZE),
            |index, _| {
                visited.push(index);
                Ok(())
            },
            super::super::VmoMapMode::SharedRead,
        )
        .unwrap();
        assert_eq!(visited.as_slice(), &[1]);
        assert_eq!(vmo.committed_pages(), 1);
    }
}
