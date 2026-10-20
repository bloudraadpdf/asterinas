// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define SET_VMA 0x53564d41

static int name_range(void *address, size_t size, const char *name)
{
	return syscall(SYS_prctl, SET_VMA, 0, address, size, name);
}

static void check_label(void *address, const char *label, size_t span)
{
	FILE *maps = fopen("/proc/self/maps", "r");
	assert(maps != NULL);
	char *line = NULL;
	size_t capacity = 0;
	int found = 0;
	while (getline(&line, &capacity, maps) > 0) {
		unsigned long start, end;
		assert(sscanf(line, "%lx-%lx", &start, &end) == 2);
		if ((uintptr_t)address < start || (uintptr_t)address >= end)
			continue;
		if (label != NULL)
			assert(strstr(line, label) != NULL);
		else
			assert(strstr(line, "[anon:") == NULL);
		if (span != 0)
			assert(start == (uintptr_t)address &&
			       end - start == span);
		found = 1;
		break;
	}
	assert(found && fclose(maps) == 0);
	free(line);
}

int main(void)
{
	size_t page = sysconf(_SC_PAGESIZE);
	char *memory = mmap(NULL, 5 * page, PROT_READ | PROT_WRITE,
			    MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	assert(memory != MAP_FAILED);
	assert(name_range(memory, 5 * page, "outer") == 0);
	assert(name_range(memory + page, 1, "inner") == 0);
	check_label(memory, "[anon:outer]", page);
	check_label(memory + page, "[anon:inner]", page);
	check_label(memory + 2 * page, "[anon:outer]", 3 * page);
	assert(mprotect(memory + page, page, PROT_READ) == 0);
	check_label(memory + page, "[anon:inner]", page);
	assert(mprotect(memory + page, page, PROT_READ | PROT_WRITE) == 0);
	char same_name[] = "outer";
	assert(name_range(memory + page, page, same_name) == 0);
	check_label(memory, "[anon:outer]", 5 * page);

	pid_t child = fork();
	assert(child >= 0);
	if (child == 0) {
		check_label(memory, "[anon:outer]", 5 * page);
		assert(name_range(memory, page, "child") == 0);
		_exit(0);
	}
	int status;
	assert(waitpid(child, &status, 0) == child && WIFEXITED(status) &&
	       WEXITSTATUS(status) == 0);
	check_label(memory, "[anon:outer]", 5 * page);
	assert(name_range(memory, 5 * page, NULL) == 0);
	check_label(memory, NULL, 0);
	assert(name_range(memory + 1, page, "bad") == -1 && errno == EINVAL);
	assert(name_range(memory, SIZE_MAX, "bad") == -1 && errno == EINVAL);
	assert(name_range(memory, 0, "valid") == 0);
	const char *invalid[] = { "bad\\", "bad`",  "bad$", "bad[",
				  "bad]",  "bad\n", "\x80" };
	for (size_t i = 0; i < sizeof(invalid) / sizeof(invalid[0]); i++)
		assert(name_range(memory, 0, invalid[i]) == -1 &&
		       errno == EINVAL);
	assert(syscall(SYS_prctl, SET_VMA, 1, memory, page, "bad") == -1 &&
	       errno == EINVAL);
	assert(name_range(memory, page, "") == 0);
	check_label(memory, "[anon:]", page);
	char long_name[81];
	memset(long_name, 'a', sizeof(long_name));
	long_name[79] = '\0';
	assert(name_range(memory, page, long_name) == 0);
	long_name[79] = 'a';
	long_name[80] = '\0';
	assert(name_range(memory, page, long_name) == -1 && errno == EINVAL);
	assert(name_range(memory, page, (const char *)1) == -1 &&
	       errno == EFAULT);
	assert(munmap(memory + 2 * page, page) == 0);
	assert(name_range(memory, 5 * page, "across hole") == -1 &&
	       errno == ENOMEM);
	check_label(memory, "[anon:across hole]", 2 * page);
	check_label(memory + 3 * page, "[anon:across hole]", 2 * page);
	assert(munmap(memory, 2 * page) == 0);
	assert(munmap(memory + 3 * page, 2 * page) == 0);

	memory = mmap(NULL, page, PROT_READ | PROT_WRITE,
		      MAP_SHARED | MAP_ANONYMOUS, -1, 0);
	assert(memory != MAP_FAILED && name_range(memory, page, "shared") == 0);
	check_label(memory, "[anon_shmem:shared]", page);
	char *destination =
		mmap(NULL, page, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	assert(destination != MAP_FAILED);
	assert(mremap(memory, page, page, MREMAP_MAYMOVE | MREMAP_FIXED,
		      destination) == destination);
	check_label(destination, "[anon_shmem:shared]", page);
	assert(munmap(destination, page) == 0);
	char path[] = "/tmp/vma-name-XXXXXX";
	int file = mkstemp(path);
	assert(file >= 0 && unlink(path) == 0 && ftruncate(file, page) == 0);
	memory = mmap(NULL, page, PROT_READ, MAP_PRIVATE, file, 0);
	assert(memory != MAP_FAILED);
	assert(name_range(memory, page, "file") == -1 && errno == EBADF);
	assert(munmap(memory, page) == 0 && close(file) == 0);
	puts("PASS VMA names, range errors, splitting, merging, fork and shared labels");
	return 0;
}
