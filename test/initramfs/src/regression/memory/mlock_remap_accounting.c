// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <stdio.h>
#include <sys/mman.h>
#include <unistd.h>

#include "mlock_common.h"

int main(void)
{
	long page = sysconf(_SC_PAGESIZE);
	assert(page > 0 && locked_bytes() == 0);
	char *memory = mmap(NULL, page, PROT_READ | PROT_WRITE,
			    MAP_PRIVATE | MAP_ANONYMOUS | MAP_LOCKED, -1, 0);
	assert(memory != MAP_FAILED && locked_bytes() == page);
	char *moved;
	moved = mremap(memory, page, page, MREMAP_MAYMOVE | MREMAP_DONTUNMAP);
	assert(moved != MAP_FAILED && locked_bytes() == page);
	assert(madvise(memory, page, MADV_DONTNEED) == 0);
	errno = 0;
	assert(madvise(moved, page, MADV_DONTNEED) == -1 && errno == EINVAL);
	assert(munmap(memory, page) == 0 && locked_bytes() == page);
	assert(munmap(moved, page) == 0 && locked_bytes() == 0);
	puts("PASS relocated locked-page accounting without phantom credit");
	return 0;
}
