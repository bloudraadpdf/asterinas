// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE

#include <fcntl.h>
#include <sched.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/wait.h>
#include <unistd.h>

#include "../../common/test.h"

FN_SETUP(tmpfs_limit_mountpoints)
{
	cpu_set_t cpus;
	CHECK(sched_getaffinity(0, sizeof(cpus), &cpus));
	fprintf(stderr, "tmpfs_cpus: %d\n", CPU_COUNT(&cpus));
	CHECK(unshare(CLONE_NEWNS));
	CHECK(mount(NULL, "/", NULL, MS_PRIVATE | MS_REC, NULL));
	CHECK_WITH(mkdir("/tmpfs_limits", 0755), _ret == 0 || errno == EEXIST);
	CHECK(mkdir("/tmpfs_limits/mode", 0755));
	CHECK(mkdir("/tmpfs_limits/pages", 0755));
	CHECK(mkdir("/tmpfs_limits/inodes", 0755));
	CHECK(mkdir("/tmpfs_limits/mapped", 0755));
	CHECK(mkdir("/tmpfs_limits/partial", 0755));
	CHECK(mkdir("/tmpfs_limits/sparse", 0755));
	CHECK(mkdir("/tmpfs_limits/open", 0755));
	CHECK(mkdir("/tmpfs_limits/concurrent", 0755));
}
END_SETUP()

FN_TEST(tmpfs_applies_root_mode_and_owner)
{
	CHECK(mount("tmpfs", "/tmpfs_limits/mode", "tmpfs",
		    MS_NOSUID | MS_NODEV,
		    "size=4k,nr_inodes=4,mode=1777,uid=123,gid=456"));
	struct stat metadata;
	CHECK(stat("/tmpfs_limits/mode", &metadata));
	TEST_RES(metadata.st_mode & 07777, _ret == 01777);
	TEST_RES(metadata.st_uid, _ret == 123);
	TEST_RES(metadata.st_gid, _ret == 456);
}
END_TEST()

FN_TEST(tmpfs_enforces_page_limit_and_releases_truncated_pages)
{
	CHECK(mount("tmpfs", "/tmpfs_limits/pages", "tmpfs", 0,
		    "size=4k,nr_inodes=4"));
	int file =
		CHECK(open("/tmpfs_limits/pages/file", O_CREAT | O_RDWR, 0600));
	char page[4096] = { 1 };
	CHECK_WITH(write(file, page, sizeof(page)), _ret == sizeof(page));
	TEST_ERRNO(pwrite(file, page, sizeof(page), sizeof(page)), ENOSPC);
	CHECK(ftruncate(file, 0));
	TEST_RES(pwrite(file, page, sizeof(page), sizeof(page)),
		 _ret == sizeof(page));
	CHECK(close(file));
}
END_TEST()

FN_TEST(tmpfs_enforces_inode_limit_and_releases_unlinked_inodes)
{
	CHECK(mount("tmpfs", "/tmpfs_limits/inodes", "tmpfs", 0,
		    "size=4k,nr_inodes=2"));
	int file = CHECK(
		open("/tmpfs_limits/inodes/first", O_CREAT | O_RDWR, 0600));
	CHECK(close(file));
	TEST_ERRNO(open("/tmpfs_limits/inodes/second", O_CREAT | O_RDWR, 0600),
		   ENOSPC);
	CHECK(unlink("/tmpfs_limits/inodes/first"));
	file = CHECK(
		open("/tmpfs_limits/inodes/second", O_CREAT | O_RDWR, 0600));
	CHECK(close(file));
}
END_TEST()

FN_TEST(tmpfs_shared_mapping_cannot_bypass_page_limit)
{
	CHECK(mount("tmpfs", "/tmpfs_limits/mapped", "tmpfs", 0,
		    "size=4k,nr_inodes=4"));
	int file = CHECK(
		open("/tmpfs_limits/mapped/file", O_CREAT | O_RDWR, 0600));
	CHECK(ftruncate(file, 8192));
	volatile char *mapping = CHECK_WITH(
		mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, file, 0),
		_ret != MAP_FAILED);
	mapping[0] = 1;
	pid_t child = CHECK(fork());
	if (child == 0) {
		mapping[4096] = 2;
		_exit(EXIT_SUCCESS);
	}
	int status;
	CHECK_WITH(waitpid(child, &status, 0), _ret == child);
	TEST_RES(status, WIFSIGNALED(_ret) && WTERMSIG(_ret) == SIGBUS);
	CHECK(munmap((void *)mapping, 8192));
	CHECK(close(file));
}
END_TEST()

FN_TEST(tmpfs_partial_write_preserves_file_size_and_accounting)
{
	CHECK(mount("tmpfs", "/tmpfs_limits/partial", "tmpfs", 0,
		    "size=4k,nr_inodes=4"));
	int file = CHECK(
		open("/tmpfs_limits/partial/file", O_CREAT | O_RDWR, 0600));
	char pages[8192] = { 1 };
	TEST_RES(write(file, pages, sizeof(pages)), _ret == 4096);
	struct stat metadata;
	CHECK(fstat(file, &metadata));
	TEST_RES(metadata.st_size, _ret == 4096);
	struct statfs usage;
	CHECK(fstatfs(file, &usage));
	TEST_RES(usage.f_blocks, _ret == 1);
	TEST_RES(usage.f_bfree, _ret == 0);
	CHECK(ftruncate(file, 0));
	CHECK(fstatfs(file, &usage));
	TEST_RES(usage.f_bfree, _ret == 1);
	CHECK(close(file));
}
END_TEST()

FN_TEST(tmpfs_sparse_reads_do_not_consume_page_budget)
{
	CHECK(mount("tmpfs", "/tmpfs_limits/sparse", "tmpfs", 0,
		    "size=4k,nr_inodes=4"));
	int file = CHECK(
		open("/tmpfs_limits/sparse/file", O_CREAT | O_RDWR, 0600));
	CHECK(ftruncate(file, 12288));
	char page[4096];
	CHECK_WITH(pread(file, page, sizeof(page), 4096), _ret == sizeof(page));
	CHECK_WITH(pread(file, page, sizeof(page), 8192), _ret == sizeof(page));
	for (size_t i = 0; i < sizeof(page); i++)
		CHECK_WITH(page[i], _ret == 0);
	TEST_RES(pwrite(file, page, sizeof(page), 0), _ret == sizeof(page));
	CHECK(close(file));
}
END_TEST()

FN_TEST(tmpfs_open_unlinked_inode_keeps_its_reservation)
{
	CHECK(mount("tmpfs", "/tmpfs_limits/open", "tmpfs", 0,
		    "size=4k,nr_inodes=2"));
	int first =
		CHECK(open("/tmpfs_limits/open/first", O_CREAT | O_RDWR, 0600));
	CHECK(unlink("/tmpfs_limits/open/first"));
	TEST_ERRNO(open("/tmpfs_limits/open/second", O_CREAT | O_RDWR, 0600),
		   ENOSPC);
	CHECK(close(first));
	int second = CHECK(
		open("/tmpfs_limits/open/second", O_CREAT | O_RDWR, 0600));
	CHECK(close(second));
}
END_TEST()

FN_TEST(tmpfs_concurrent_writers_share_page_budget)
{
	CHECK(mount("tmpfs", "/tmpfs_limits/concurrent", "tmpfs", 0,
		    "size=4k,nr_inodes=8"));
	int gate[2];
	CHECK(pipe(gate));
	pid_t children[4];
	for (int i = 0; i < 4; i++) {
		children[i] = CHECK(fork());
		if (children[i] == 0) {
			char path[64];
			CHECK_WITH(snprintf(path, sizeof(path),
					    "/tmpfs_limits/concurrent/%d", i),
				   _ret > 0 && _ret < sizeof(path));
			int file = CHECK(open(path, O_CREAT | O_RDWR, 0600));
			char byte;
			CHECK_WITH(read(gate[0], &byte, 1), _ret == 1);
			char page[4096] = { 1 };
			ssize_t written = write(file, page, sizeof(page));
			int failure = errno;
			CHECK(close(file));
			if (written == sizeof(page))
				_exit(0);
			CHECK_WITH(written, _ret == -1 && failure == ENOSPC);
			_exit(1);
		}
	}
	CHECK_WITH(write(gate[1], "rrrr", 4), _ret == 4);
	int successes = 0;
	for (int i = 0; i < 4; i++) {
		int status;
		CHECK_WITH(waitpid(children[i], &status, 0),
			   _ret == children[i] && WIFEXITED(status));
		CHECK_WITH(WEXITSTATUS(status), _ret == 0 || _ret == 1);
		successes += WEXITSTATUS(status) == 0;
	}
	TEST_RES(successes, _ret == 1);
	CHECK(close(gate[0]));
	CHECK(close(gate[1]));
}
END_TEST()
