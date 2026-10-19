/* SPDX-License-Identifier: MPL-2.0 */
#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <stdint.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

#define GET_AUXV 0x41555856

int main(int argc, char **argv)
{
	uintptr_t vector[128] = { 0 };
	long size = syscall(SYS_prctl, GET_AUXV, vector, sizeof(vector), 0, 0);
	assert(size > 0 && (size_t)size <= sizeof(vector));
	assert(syscall(SYS_prctl, GET_AUXV, (void *)1, 0, 0, 0) == size);
	uintptr_t prefix[2];
	assert(syscall(SYS_prctl, GET_AUXV, prefix, sizeof(prefix), 0, 0) ==
	       size);
	assert(!memcmp(prefix, vector, sizeof(prefix)));
	errno = 0;
	assert(syscall(SYS_prctl, GET_AUXV, (void *)1, 16, 1, 0) == -1);
	assert(errno == EINVAL);
	errno = 0;
	assert(syscall(SYS_prctl, GET_AUXV, (void *)1, 16, 0, 0) == -1);
	assert(errno == EFAULT);
	long page = sysconf(_SC_PAGESIZE);
	char *memory = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE,
			    MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	assert(memory != MAP_FAILED);
	assert(mprotect(memory + page, page, PROT_NONE) == 0);
	errno = 0;
	assert(syscall(SYS_prctl, GET_AUXV, memory + page - 8, 16, 0, 0) == -1);
	assert(errno == EFAULT);
	assert(munmap(memory, 2 * page) == 0);
	const char *execfn = (const char *)getauxval(AT_EXECFN);
	assert(execfn != NULL);
	if (argc == 2) {
		assert(!strcmp(argv[0], "custom argv zero"));
		assert(!strcmp(execfn, argv[1]));
		return 0;
	}
	char *arguments[] = { "custom argv zero", argv[0], NULL };
	execve(argv[0], arguments, NULL);
	assert(0);
}
