// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>
#ifdef __GLIBC__
#include <sys/rseq.h>
#endif

static long call(void *area, unsigned size, unsigned flags, unsigned signature)
{
	return syscall(SYS_rseq, area, size, flags, signature);
}

int main(void)
{
#ifdef __GLIBC__
	void *libc_area = (char *)__builtin_thread_pointer() + __rseq_offset;
	if (__rseq_size && ((uint32_t *)libc_area)[1] < UINT32_MAX - 1)
		assert(call(libc_area, __rseq_size < 32 ? 32 : __rseq_size, 1,
			    RSEQ_SIG) == 0);
#endif
	size_t page = sysconf(_SC_PAGESIZE);
	char *memory = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE,
			    MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	assert(memory != MAP_FAILED &&
	       mprotect(memory + page, page, PROT_NONE) == 0);
	uint32_t *area = (void *)(memory + page - 64);
	assert(call(area, 128, 0, 0x53053053) == 0);
	assert(area[0] == area[1] && area[1] < UINT32_MAX - 1);
	errno = 0;
	assert(call(area, 128, 0, 0x53053053) == -1 && errno == EBUSY);
	pid_t child = fork();
	assert(child >= 0);
	if (!child) {
		assert(call(area, 128, 0, 0x53053053) == -1 && errno == EBUSY);
		assert(call(area, 128, 1, 0x53053053) == 0);
		_exit(0);
	}
	int status;
	assert(waitpid(child, &status, 0) == child && WIFEXITED(status) &&
	       WEXITSTATUS(status) == 0);
	errno = 0;
	assert(call(area, 128, 1, 0x53053052) == -1 && errno == EPERM);
	assert(call(area, 128, 1, 0x53053053) == 0);
	assert(area[0] == 0 && area[1] == UINT32_MAX);
	assert(munmap(memory, 2 * page) == 0);
	puts("PASS rseq reserved-tail access, fork inheritance and failed unregister lifetime");
	return 0;
}
