// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <unistd.h>

#include "mlock_common.h"

static void wait_success(pid_t child)
{
	int status;
	assert(waitpid(child, &status, 0) == child);
	assert(WIFEXITED(status) && WEXITSTATUS(status) == 0);
}

static void check_file_truncation(long page)
{
	char name[] = "/tmp/mlock_file_XXXXXX";
	int file = mkstemp(name);
	assert(file >= 0 && unlink(name) == 0);
	assert(ftruncate(file, page) == 0);
	assert(pwrite(file, "a", 1, 0) == 1);
	char *memory =
		mmap(NULL, page, PROT_READ | PROT_WRITE, MAP_SHARED, file, 0);
	assert(memory != MAP_FAILED && mlock(memory, page) == 0);
	assert(locked_bytes() == page);
	assert(ftruncate(file, 0) == 0);
	errno = 0;
	assert(mlock(memory, page) == -1 && errno == ENOMEM);
	unsigned char state;
	assert(mincore(memory, page, &state) == 0 && !(state & 1));
	assert(locked_bytes() == page);
	assert(ftruncate(file, page) == 0 && pwrite(file, "z", 1, 0) == 1);
	assert(memory[0] == 'z');
	assert(mincore(memory, page, &state) == 0 && (state & 1));
	assert(munmap(memory, page) == 0 && locked_bytes() == 0);
	assert(close(file) == 0);
}

static void check_limits_and_lifetimes(long page)
{
	struct rlimit limit;
	assert(getrlimit(RLIMIT_MEMLOCK, &limit) == 0);
	assert(limit.rlim_max >= 2 * page);
	limit.rlim_cur = 2 * page;
	assert(setrlimit(RLIMIT_MEMLOCK, &limit) == 0);
	if (geteuid() == 0) {
		assert(setgid(65534) == 0 && setuid(65534) == 0);
	}
	char *memory = mmap(NULL, 4 * page, PROT_READ | PROT_WRITE,
			    MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	assert(memory != MAP_FAILED && locked_bytes() == 0);
	assert(mlock(memory, page) == 0 && locked_bytes() == page);
	assert(mlock(memory, page) == 0 && locked_bytes() == page);
	assert(mlock(memory + 1, page) == 0 && locked_bytes() == 2 * page);
	errno = 0;
	assert(mlock(memory + 2 * page, page) == -1 && errno == ENOMEM);
	errno = 0;
	assert(madvise(memory, page, MADV_DONTNEED) == -1 && errno == EINVAL);
	assert(munlock(memory, page) == 0 && locked_bytes() == page);
	assert(mlock(memory + 2 * page, page) == 0 &&
	       locked_bytes() == 2 * page);
	assert(munmap(memory + page, page) == 0 && locked_bytes() == page);
	assert(mmap(memory + page, page, PROT_READ | PROT_WRITE,
		    MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1,
		    0) == memory + page);
	assert(locked_bytes() == page);
	memory[2 * page] = 42;
	pid_t child = fork();
	assert(child >= 0);
	if (child == 0) {
		assert(locked_bytes() == 0);
		assert(madvise(memory + 2 * page, page, MADV_DONTNEED) == 0);
		assert(memory[2 * page] == 0);
		_exit(0);
	}
	wait_success(child);
	assert(memory[2 * page] == 42 && locked_bytes() == page);
	char *moved = mremap(memory + 2 * page, page, 2 * page, MREMAP_MAYMOVE);
	assert(moved != MAP_FAILED && locked_bytes() == 2 * page);
	unsigned char states[2];
	assert(mincore(moved, 2 * page, states) == 0 && (states[0] & 1) &&
	       (states[1] & 1));
	assert(munlock(moved, page) == 0 && locked_bytes() == page);
	assert(madvise(moved, page, MADV_DONTNEED) == 0);
	assert(mremap(moved + page, page, page, MREMAP_MAYMOVE | MREMAP_FIXED,
		      memory + 2 * page) == memory + 2 * page);
	assert(locked_bytes() == page);
	assert(munmap(memory, 4 * page) == 0 && locked_bytes() == 0);
	assert(munmap(moved, 2 * page) == 0);

	memory =
		mmap(NULL, page, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	assert(memory != MAP_FAILED);
	errno = 0;
	assert(mlock(memory, page) == -1 && errno == ENOMEM);
	assert(munlock(memory, page) == 0 && locked_bytes() == 0);
	assert(munmap(memory, page) == 0);
	check_file_truncation(page);
	memory = mmap(NULL, page, PROT_READ | PROT_WRITE,
		      MAP_PRIVATE | MAP_ANONYMOUS | MAP_LOCKED, -1, 0);
	assert(memory != MAP_FAILED && locked_bytes() == page);
	assert(mincore(memory, page, states) == 0 && (states[0] & 1));
	assert(munmap(memory, page) == 0 && locked_bytes() == 0);
	limit.rlim_cur = 0;
	assert(setrlimit(RLIMIT_MEMLOCK, &limit) == 0);
	errno = 0;
	assert(mlock(NULL, 0) == -1 && errno == EPERM);
	assert(munlock(NULL, 0) == 0);
}

int main(void)
{
	long page = sysconf(_SC_PAGESIZE);
	pid_t child = fork();
	assert(child >= 0);
	if (child == 0) {
		check_limits_and_lifetimes(page);
		_exit(0);
	}
	wait_success(child);
	return 0;
}
