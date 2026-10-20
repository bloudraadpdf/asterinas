// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE

#include <fcntl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

#include "../../common/test.h"

#ifndef SYS_fchmodat2
#define SYS_fchmodat2 452
#endif

static char directory[] = "/tmp/symlink_chmod_XXXXXX";
static int dirfd;

FN_SETUP(prepare)
{
	CHECK_WITH(mkdtemp(directory), _ret != NULL);
	dirfd = CHECK(open(directory, O_RDONLY | O_DIRECTORY));
	int fd = CHECK(openat(dirfd, "file", O_CREAT | O_RDWR | O_EXCL, 0600));
	CHECK(close(fd));
	CHECK(symlinkat("file", dirfd, "link"));
	CHECK(symlinkat("missing", dirfd, "dangling"));
}
END_SETUP()

FN_TEST(nofollow_rejects_symlink_mode_changes)
{
	struct stat before;
	struct stat after = { 0 };
	CHECK(fstatat(dirfd, "link", &before, AT_SYMLINK_NOFOLLOW));
	TEST_ERRNO(fchmodat(dirfd, "link", 0600, AT_SYMLINK_NOFOLLOW),
		   EOPNOTSUPP);
	TEST_ERRNO(syscall(SYS_fchmodat2, dirfd, "link", 0600,
			   AT_SYMLINK_NOFOLLOW),
		   EOPNOTSUPP);
	TEST_SUCC(fstatat(dirfd, "link", &after, AT_SYMLINK_NOFOLLOW));
	TEST_RES(after.st_mode, _ret == before.st_mode);
	TEST_SUCC(fstatat(dirfd, "file", &after, 0));
	TEST_RES(after.st_mode & 07777, _ret == 0600);
	TEST_ERRNO(fchmodat(dirfd, "dangling", 0600, AT_SYMLINK_NOFOLLOW),
		   EOPNOTSUPP);
	TEST_ERRNO(fchmodat(dirfd, "dangling", 0600, 0), ENOENT);
}
END_TEST()

FN_TEST(nofollow_allows_regular_file_mode_changes)
{
	struct stat statbuf = { 0 };
	TEST_SUCC(fchmodat(dirfd, "file", 0640, AT_SYMLINK_NOFOLLOW));
	TEST_SUCC(fstatat(dirfd, "file", &statbuf, 0));
	TEST_RES(statbuf.st_mode & 07777, _ret == 0640);
	TEST_SUCC(fchmodat(dirfd, "link", 0600, 0));
	TEST_SUCC(fstatat(dirfd, "file", &statbuf, 0));
	TEST_RES(statbuf.st_mode & 07777, _ret == 0600);
}
END_TEST()

FN_TEST(empty_path_rejects_symlink_mode_changes)
{
	int path_fd = CHECK(openat(dirfd, "link", O_PATH | O_NOFOLLOW));
	struct stat before;
	struct stat after = { 0 };
	CHECK(fstat(path_fd, &before));
	TEST_ERRNO(syscall(SYS_fchmodat2, path_fd, "", 0600, AT_EMPTY_PATH),
		   EOPNOTSUPP);
	TEST_ERRNO(fchmod(path_fd, 0600), EBADF);
	TEST_SUCC(fstat(path_fd, &after));
	TEST_RES(after.st_mode, _ret == before.st_mode);
	TEST_SUCC(close(path_fd));
}
END_TEST()

FN_SETUP(cleanup)
{
	CHECK(unlinkat(dirfd, "dangling", 0));
	CHECK(unlinkat(dirfd, "link", 0));
	CHECK(unlinkat(dirfd, "file", 0));
	CHECK(close(dirfd));
	CHECK(rmdir(directory));
}
END_SETUP()
