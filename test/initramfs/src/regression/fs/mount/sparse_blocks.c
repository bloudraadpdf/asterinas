// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE

#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/wait.h>
#include <unistd.h>

static void check_blocks_at(int file, off_t size, blkcnt_t pages,
			    long page_size, int line)
{
	struct stat metadata;
	assert(fstat(file, &metadata) == 0);
	if (metadata.st_size != size ||
	    metadata.st_blocks != pages * (page_size / 512)) {
		fprintf(stderr,
			"sparse_blocks.c:%d: size expected=%jd actual=%jd; "
			"blocks expected=%jd actual=%jd; pages=%jd page_size=%ld\n",
			line, (intmax_t)size, (intmax_t)metadata.st_size,
			(intmax_t)(pages * (page_size / 512)),
			(intmax_t)metadata.st_blocks, (intmax_t)pages,
			page_size);
		fflush(stderr);
	}
	assert(metadata.st_size == size);
	assert(metadata.st_blocks == pages * (page_size / 512));
}

#define check_blocks(file, size, pages, page_size) \
	check_blocks_at(file, size, pages, page_size, __LINE__)

int main(void)
{
	assert(unshare(CLONE_NEWNS) == 0);
	assert(mount(NULL, "/", NULL, MS_PRIVATE | MS_REC, NULL) == 0);
	char directory[] = "/tmp/sparse-blocks-XXXXXX";
	assert(mkdtemp(directory) != NULL);
	assert(mount("tmpfs", directory, "tmpfs", 0, "size=1m") == 0);
	char path[128];
	assert(snprintf(path, sizeof(path), "%s/file", directory) > 0);
	int file = open(path, O_CREAT | O_EXCL | O_RDWR, 0600);
	assert(file >= 0 && unlink(path) == 0);
	long page = sysconf(_SC_PAGESIZE);
	assert(page > 0 && page % 512 == 0);
	const off_t distant = 16 * 1024 * 1024;
	assert(ftruncate(file, 2 * distant) == 0);
	check_blocks(file, 2 * distant, 0, page);
	assert(pwrite(file, "a", 1, 0) == 1);
	assert(pwrite(file, "b", 1, distant) == 1);
	check_blocks(file, 2 * distant, 2, page);
	assert(ftruncate(file, 4 * distant) == 0);
	check_blocks(file, 4 * distant, 2, page);

	volatile unsigned char *mapping = mmap(NULL, 2 * page,
					       PROT_READ | PROT_WRITE,
					       MAP_SHARED, file, 4 * page);
	assert(mapping != MAP_FAILED && mapping[0] == 0);
	check_blocks(file, 4 * distant, 3, page);
	mapping[page] = 5;
	mapping[2 * page - 1] = 7;
	check_blocks(file, 4 * distant, 4, page);
	assert(fallocate(file, FALLOC_FL_KEEP_SIZE, 4 * distant + page,
			 2 * page) == 0);
	check_blocks(file, 4 * distant, 6, page);

	pid_t child = fork();
	assert(child >= 0);
	if (child == 0) {
		volatile unsigned char *past_eof = mmap(NULL, page, PROT_READ,
							MAP_SHARED, file,
							4 * distant + page);
		assert(past_eof != MAP_FAILED);
		_exit(past_eof[0]);
	}
	int status;
	assert(waitpid(child, &status, 0) == child);
	if (!WIFSIGNALED(status) || WTERMSIG(status) != SIGBUS) {
		fprintf(stderr,
			"past-EOF access: wait status=%#x, expected SIGBUS\n",
			status);
		fflush(stderr);
	}
	assert(WIFSIGNALED(status) && WTERMSIG(status) == SIGBUS);
	assert(ftruncate(file, 4 * distant) == 0);
	check_blocks(file, 4 * distant, 4, page);
	assert(ftruncate(file, 8 * page) == 0);
	check_blocks(file, 8 * page, 3, page);

	assert(fallocate(file, FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE,
			 4 * page, page) == 0);
	check_blocks(file, 8 * page, 2, page);
	assert(mapping[0] == 0 && mapping[page] == 5);
	check_blocks(file, 8 * page, 3, page);
	assert(fallocate(file, FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE,
			 5 * page + 1, page - 2) == 0);
	assert(mapping[page] == 5 && mapping[page + 1] == 0 &&
	       mapping[2 * page - 1] == 7);
	check_blocks(file, 8 * page, 3, page);
	assert(fallocate(file, 0, 8 * page, 2 * page) == 0);
	check_blocks(file, 10 * page, 5, page);
	assert(munmap((void *)mapping, 2 * page) == 0);
	assert(ftruncate(file, 0) == 0);
	check_blocks(file, 0, 0, page);
	assert(close(file) == 0 && umount(directory) == 0);

	char options[64];
	assert(snprintf(options, sizeof(options), "size=%ld", 3 * page) > 0);
	assert(mount("tmpfs", directory, "tmpfs", 0, options) == 0);
	file = open(path, O_CREAT | O_EXCL | O_RDWR, 0600);
	assert(file >= 0 && unlink(path) == 0);
	int occupied = open(path, O_CREAT | O_EXCL | O_RDWR, 0600);
	assert(occupied >= 0 && unlink(path) == 0);
	assert(pwrite(occupied, "x", 1, 0) == 1);
	assert(pwrite(file, "a", 1, 0) == 1);
	assert(fallocate(file, FALLOC_FL_KEEP_SIZE, 0, 3 * page) == -1 &&
	       errno == ENOSPC);
	check_blocks(file, 1, 1, page);
	char byte;
	assert(pread(file, &byte, 1, 0) == 1 && byte == 'a');
	struct statfs usage;
	assert(fstatfs(file, &usage) == 0 && usage.f_bfree == 1);
	assert(pwrite(file, "b", 1, page) == 1);
	check_blocks(file, page + 1, 2, page);
	assert(close(file) == 0 && close(occupied) == 0);
	assert(umount(directory) == 0);
	assert(rmdir(directory) == 0);
	puts("PASS sparse allocation, mmap faults, preallocation, punch and rollback");
	return 0;
}
