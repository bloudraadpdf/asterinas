// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <assert.h>
#include <signal.h>
#include <stdio.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void)
{
	size_t page = sysconf(_SC_PAGESIZE), size = 4 * 1024 * 1024;
	char *space =
		mmap(NULL, size, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	assert(space != MAP_FAILED && munmap(space, size) == 0);
	volatile char *top = space + size - 2 * page;
	assert(mmap((void *)top, page, PROT_READ | PROT_WRITE,
		    MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE |
			    MAP_GROWSDOWN,
		    -1, 0) == top);
	top[0] = 43;
	top[-1] = 44;
	assert(top[0] == 43 && top[-1] == 44);
	struct rlimit saved, limit;
	assert(getrlimit(RLIMIT_DATA, &saved) == 0);
	limit = saved;
	limit.rlim_cur = 1;
	assert(setrlimit(RLIMIT_DATA, &limit) == 0);
	top[-2 * page] = 45;
	assert(top[-2 * page] == 45);
	assert(setrlimit(RLIMIT_DATA, &saved) == 0);
	pid_t child = fork();
	assert(child >= 0);
	if (!child) {
		assert(getrlimit(RLIMIT_STACK, &limit) == 0);
		limit.rlim_cur = page;
		assert(setrlimit(RLIMIT_STACK, &limit) == 0);
		top[-3 * page] = 46;
		_exit(1);
	}
	int status;
	assert(waitpid(child, &status, 0) == child && WIFSIGNALED(status) &&
	       WTERMSIG(status) == SIGSEGV);
	top[-3 * page] = 47;
	assert(top[-3 * page] == 47 && top[-2 * page] == 45 && top[-1] == 44 &&
	       top[0] == 43);
	assert(munmap((void *)(top - 3 * page), 4 * page) == 0);
	puts("PASS sparse grow-down mapping, data exemption and forked stack limit");
	return 0;
}
