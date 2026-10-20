// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#define PRESSURE_BYTES ((size_t)1024 * 1024 * 1024)
#define RECOVERY_BYTES ((size_t)16 * 1024 * 1024)

static void touch(volatile unsigned char *memory, size_t size, size_t page)
{
	for (size_t offset = 0; offset < size; offset += page)
		memory[offset] = (unsigned char)(offset / page);
}

int main(void)
{
	long page = sysconf(_SC_PAGESIZE);
	assert(page > 0);
	alarm(30);
	pid_t child = fork();
	assert(child >= 0);
	if (child == 0) {
		volatile unsigned char *memory =
			mmap(NULL, PRESSURE_BYTES, PROT_READ | PROT_WRITE,
			     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
		if (memory == MAP_FAILED) {
			assert(errno == ENOMEM);
			_exit(0);
		}
		touch(memory, PRESSURE_BYTES, page);
		assert(munmap((void *)memory, PRESSURE_BYTES) == 0);
		_exit(0);
	}
	int status;
	assert(waitpid(child, &status, 0) == child);
	assert((WIFEXITED(status) && WEXITSTATUS(status) == 0) ||
	       (WIFSIGNALED(status) && WTERMSIG(status) == SIGKILL));

	volatile unsigned char *memory =
		mmap(NULL, RECOVERY_BYTES, PROT_READ | PROT_WRITE,
		     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	assert(memory != MAP_FAILED);
	touch(memory, RECOVERY_BYTES, page);
	assert(munmap((void *)memory, RECOVERY_BYTES) == 0);
	child = fork();
	assert(child >= 0);
	if (child == 0)
		_exit(0);
	assert(waitpid(child, &status, 0) == child);
	assert(WIFEXITED(status) && WEXITSTATUS(status) == 0);
	alarm(0);
	puts("PASS kernel and parent progress after bounded anonymous pressure");
	return 0;
}
