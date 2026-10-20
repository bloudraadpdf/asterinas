// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void)
{
	long page = sysconf(_SC_PAGESIZE);
	char *memory = mmap(NULL, 3 * page, PROT_READ | PROT_WRITE,
			    MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	assert(memory != MAP_FAILED);
	unsigned char states[3];
	assert(mincore(memory, 3 * page, states) == 0);
	assert(!(states[0] & 1) && !(states[1] & 1) && !(states[2] & 1));
	memory[page] = 1;
	assert(mincore(memory, 3 * page, states) == 0);
	assert(!(states[0] & 1) && (states[1] & 1) && !(states[2] & 1));
	assert(mprotect(memory + page, page, PROT_NONE) == 0);
	assert(mincore(memory + page, 1, states) == 0 && (states[0] & 1));
	assert(madvise(memory + page, page, MADV_DONTNEED) == 0);
	assert(mincore(memory + page, page, states) == 0 && !(states[0] & 1));
	errno = 0;
	assert(mincore(memory + 1, 0, states) == -1 && errno == EINVAL);
	errno = 0;
	assert(mincore(memory, page, (void *)1) == -1 && errno == EFAULT);
	assert(mincore(memory, 0, (void *)1) == 0);
	assert(munmap(memory + page, page) == 0);
	errno = 0;
	assert(mincore(memory, 3 * page, states) == -1 && errno == ENOMEM);
	assert(munmap(memory, page) == 0);
	assert(munmap(memory + 2 * page, page) == 0);

	char name[] = "/tmp/mincore_XXXXXX";
	int file = mkstemp(name);
	assert(file >= 0 && ftruncate(file, 2 * page) == 0);
	assert(pwrite(file, "x", 1, page) == 1);
	memory = mmap(NULL, 2 * page, PROT_NONE, MAP_SHARED, file, 0);
	assert(memory != MAP_FAILED);
	assert(mincore(memory, 2 * page, states) == 0);
	assert(states[1] & 1);
	if (geteuid() == 0) {
		assert(fchmod(file, 0400) == 0);
		pid_t child = fork();
		assert(child >= 0);
		if (child == 0) {
			assert(setgid(65534) == 0 && setuid(65534) == 0);
			assert(mincore(memory, 2 * page, states) == 0);
			assert((states[0] & 1) && (states[1] & 1));
			_exit(0);
		}
		int status;
		assert(waitpid(child, &status, 0) == child);
		assert(WIFEXITED(status) && WEXITSTATUS(status) == 0);
	}
	assert(munmap(memory, 2 * page) == 0);
	assert(close(file) == 0 && unlink(name) == 0);
	return 0;
}
