// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <unistd.h>

static size_t data_bytes(void)
{
	FILE *maps = fopen("/proc/self/maps", "r");
	assert(maps);
	char line[1024], perms[5];
	unsigned long start, end;
	size_t total = 0;
	uintptr_t stack = (uintptr_t)&total;
	while (fgets(line, sizeof(line), maps)) {
		assert(sscanf(line, "%lx-%lx %4s", &start, &end, perms) == 3);
		if (perms[1] == 'w' && perms[3] == 'p' &&
		    !(start <= stack && stack < end))
			total += end - start;
	}
	assert(fclose(maps) == 0);
	return total;
}

int main(void)
{
	size_t page = sysconf(_SC_PAGESIZE);
	struct rlimit saved, cap;
	assert(getrlimit(RLIMIT_DATA, &saved) == 0);
	char *region = mmap(NULL, 4 * page, PROT_READ,
			    MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	assert(region != MAP_FAILED);
	cap = saved;
	cap.rlim_cur = data_bytes() + 2 * page;
	assert(setrlimit(RLIMIT_DATA, &cap) == 0);
	assert(mprotect(region, 2 * page, PROT_READ | PROT_WRITE) == 0);
	region[0] = 41;
	errno = 0;
	assert(mprotect(region + 2 * page, page, PROT_READ | PROT_WRITE) ==
		       -1 &&
	       errno == ENOMEM);
	errno = 0;
	assert(mmap(NULL, page, PROT_READ | PROT_WRITE,
		    MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) == MAP_FAILED &&
	       errno == ENOMEM);
	assert(mprotect(region, page, PROT_READ) == 0);
	assert(mprotect(region + 2 * page, page, PROT_READ | PROT_WRITE) == 0);
	assert(mmap(region + page, page, PROT_READ | PROT_WRITE,
		    MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1,
		    0) == region + page);
	assert(region[0] == 41 && region[page] == 0);
	void *shared = mmap(NULL, page, PROT_READ | PROT_WRITE,
			    MAP_SHARED | MAP_ANONYMOUS, -1, 0);
	assert(shared != MAP_FAILED);
	assert(munmap(shared, page) == 0);
	pid_t child = fork();
	assert(child >= 0);
	if (!child) {
		assert(munmap(region + page, page) == 0);
		void *fresh = mmap(NULL, page, PROT_READ | PROT_WRITE,
				   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
		assert(fresh != MAP_FAILED);
		_exit(0);
	}
	int status;
	assert(waitpid(child, &status, 0) == child && WIFEXITED(status) &&
	       WEXITSTATUS(status) == 0);
	errno = 0;
	assert(mprotect(region, page, PROT_READ | PROT_WRITE) == -1 &&
	       errno == ENOMEM);
	assert(munmap(region, 4 * page) == 0);
	char *resize = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE,
			    MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	assert(resize != MAP_FAILED);
	errno = 0;
	assert(mremap(resize, 2 * page, 3 * page, MREMAP_MAYMOVE) ==
		       MAP_FAILED &&
	       errno == ENOMEM);
	assert(mremap(resize, 2 * page, page, 0) == resize);
	char *fresh = mmap(NULL, page, PROT_READ | PROT_WRITE,
			   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	assert(fresh != MAP_FAILED);
	assert(setrlimit(RLIMIT_DATA, &saved) == 0);
	assert(munmap(resize, page) == 0 && munmap(fresh, page) == 0);
	puts("PASS data charges through protect, replace, fork, unmap and remap");
	return 0;
}
