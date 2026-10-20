/* SPDX-License-Identifier: MPL-2.0 */

#define _GNU_SOURCE

#include <dirent.h>
#include <fcntl.h>
#include <inttypes.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

#include "../../common/test.h"

#define TEST_DIR "/tmp/aster_tmpfile_test"
#define CROSS_LINK_DIR "/ext2"
#define CROSS_LINK_NAME "cross_mount_tmpfile"
#define LINKED_NAME "linked_file"
#define LINKED_SECOND_NAME "linked_second_file"
#define LINKED_O_EXCL_NAME "linked_o_excl_file"
#define PROC_LINKED_NAME "proc_linked_file"
#define SYMLINK_NAME "tmpfile_symlink"
#define DATA "hello from tmpfile"
#define DATA_LEN (sizeof(DATA) - 1)

#define RAW_O_TMPFILE (O_TMPFILE & ~O_DIRECTORY)

static void cleanup_test_files(void)
{
	unlink(TEST_DIR "/" LINKED_NAME);
	unlink(TEST_DIR "/" LINKED_SECOND_NAME);
	unlink(TEST_DIR "/" LINKED_O_EXCL_NAME);
	unlink(TEST_DIR "/" SYMLINK_NAME);
	unlink(TEST_DIR "/" PROC_LINKED_NAME);
	unlink(CROSS_LINK_DIR "/" CROSS_LINK_NAME);
	rmdir(TEST_DIR);
}

static int fd_link_matches(int fd, const char *expected)
{
	char path[64];
	char target[256];
	int size = snprintf(path, sizeof(path), "/proc/self/fd/%d", fd);
	if (size < 0 || (size_t)size >= sizeof(path))
		return 0;
	ssize_t length = readlink(path, target, sizeof(target) - 1);
	if (length < 0)
		return 0;
	target[length] = '\0';
	return strcmp(target, expected) == 0;
}

static int timespec_equal(struct timespec left, struct timespec right)
{
	return left.tv_sec == right.tv_sec && left.tv_nsec == right.tv_nsec;
}

static int ext2_fixture_is_unavailable(void)
{
	struct stat source_stat;
	struct stat target_stat;

	CHECK(stat(TEST_DIR, &source_stat));
	int result =
		CHECK_WITH(stat(CROSS_LINK_DIR, &target_stat),
			   _ret == 0 || errno == ENOENT || errno == ENOTDIR);
	int unavailable = result < 0 ||
			  source_stat.st_dev == target_stat.st_dev;
	if (unavailable)
		fprintf(stderr,
			"tmpfile: /ext2 fixture absent or not a separate filesystem\n");
	return unavailable;
}

FN_SETUP(prepare)
{
	cleanup_test_files();
	CHECK(mkdir(TEST_DIR, 0755));
}
END_SETUP()

FN_TEST(tmpfile_open_succeeds)
{
	int fd;

	fd = TEST_SUCC(open(TEST_DIR, O_TMPFILE | O_RDWR, 0666));
	TEST_SUCC(close(fd));
}
END_TEST()

FN_TEST(directory_cookies_resume_after_each_entry)
{
	DIR *directory = TEST_RES(opendir(TEST_DIR), _ret != NULL);
	struct dirent *entry;
	long previous = 0;
	while ((entry = readdir(directory)) != NULL) {
		TEST_RES(entry->d_off, _ret > previous);
		previous = entry->d_off;
		TEST_RES(telldir(directory), _ret == previous);
	}
	TEST_SUCC(closedir(directory));
}
END_TEST()

FN_TEST(tmpfile_status_flags_survive_setfl)
{
	int fd =
		TEST_SUCC(open(TEST_DIR, O_TMPFILE | O_RDWR | O_CLOEXEC, 0600));
	int flags = TEST_SUCC(fcntl(fd, F_GETFL));
	TEST_RES(flags,
		 (flags & (O_TMPFILE | O_ACCMODE)) == (O_TMPFILE | O_RDWR));
	TEST_RES(flags, (flags & O_CLOEXEC) == 0);
	TEST_SUCC(fcntl(fd, F_SETFL, O_APPEND));
	flags = TEST_SUCC(fcntl(fd, F_GETFL));
	TEST_RES(flags, (flags & (O_TMPFILE | O_ACCMODE | O_APPEND)) ==
				(O_TMPFILE | O_RDWR | O_APPEND));
	TEST_SUCC(close(fd));
}
END_TEST()

FN_TEST(tmpfile_cannot_be_linked_after_last_unlink)
{
	int fd = TEST_SUCC(open(TEST_DIR, O_TMPFILE | O_RDWR, 0600));
	int dirfd = TEST_SUCC(open(TEST_DIR, O_RDONLY | O_DIRECTORY));
	TEST_SUCC(linkat(fd, "", dirfd, "first", AT_EMPTY_PATH));
	TEST_SUCC(unlinkat(dirfd, "first", 0));
	TEST_ERRNO(linkat(fd, "", dirfd, "again", AT_EMPTY_PATH), ENOENT);
	TEST_SUCC(close(fd));
	fd = TEST_SUCC(openat(dirfd, "regular", O_CREAT | O_RDWR, 0600));
	TEST_SUCC(unlinkat(dirfd, "regular", 0));
	TEST_ERRNO(linkat(fd, "", dirfd, "again", AT_EMPTY_PATH), ENOENT);
	TEST_SUCC(close(fd));
	TEST_SUCC(close(dirfd));
}
END_TEST()

FN_TEST(tmpfile_open_write_only_succeeds)
{
	int fd;

	fd = TEST_SUCC(open(TEST_DIR, O_TMPFILE | O_WRONLY, 0666));
	TEST_SUCC(close(fd));
}
END_TEST()

FN_TEST(tmpfile_open_read_only_returns_einval)
{
	TEST_ERRNO(open(TEST_DIR, O_TMPFILE | O_RDONLY, 0666), EINVAL);
}
END_TEST()

FN_TEST(tmpfile_open_without_o_directory_returns_einval)
{
	TEST_ERRNO(open(TEST_DIR, RAW_O_TMPFILE | O_RDWR, 0666), EINVAL);
}
END_TEST()

FN_TEST(tmpfile_open_with_o_creat_returns_einval)
{
	TEST_ERRNO(open(TEST_DIR, O_TMPFILE | O_RDWR | O_CREAT, 0666), EINVAL);
}
END_TEST()

FN_TEST(tmpfile_open_with_o_path_yields_path_fd)
{
	int fd;
	char buf[1];

	fd = TEST_SUCC(open(TEST_DIR, O_TMPFILE | O_PATH | O_RDWR, 0666));
	TEST_ERRNO(read(fd, buf, sizeof(buf)), EBADF);
	TEST_SUCC(close(fd));
}
END_TEST()

// FIXME: Add `O_TMPFILE` support for ext2.
#ifdef __asterinas__
FN_TEST(tmpfile_open_on_ext2_returns_eopnotsupp)
{
	SKIP_TEST_IF(ext2_fixture_is_unavailable());
	TEST_ERRNO(open(CROSS_LINK_DIR, O_TMPFILE | O_RDWR, 0666), EOPNOTSUPP);
}
END_TEST()
#endif

FN_TEST(tmpfile_open_with_o_excl_succeeds)
{
	int fd;

	fd = TEST_SUCC(open(TEST_DIR, O_TMPFILE | O_RDWR | O_EXCL, 0666));
	TEST_SUCC(close(fd));
}
END_TEST()

FN_TEST(tmpfile_open_symlink_with_o_nofollow_returns_enotdir)
{
	TEST_SUCC(symlink(TEST_DIR, TEST_DIR "/" SYMLINK_NAME));

	TEST_ERRNO(open(TEST_DIR "/" SYMLINK_NAME,
			O_TMPFILE | O_RDWR | O_NOFOLLOW, 0666),
		   ENOTDIR);

	TEST_SUCC(unlink(TEST_DIR "/" SYMLINK_NAME));
}
END_TEST()

FN_TEST(tmpfile_open_non_dir_returns_enotdir)
{
	TEST_ERRNO(open("/dev/null", O_TMPFILE | O_RDWR, 0666), ENOTDIR);
}
END_TEST()

FN_TEST(tmpfile_open_does_not_update_parent_timestamps)
{
	struct stat before;
	struct stat after;
	int fd;

	TEST_SUCC(stat(TEST_DIR, &before));
	fd = TEST_SUCC(open(TEST_DIR, O_TMPFILE | O_RDWR, 0666));
	TEST_SUCC(close(fd));
	TEST_SUCC(stat(TEST_DIR, &after));

	TEST_RES(timespec_equal(after.st_mtim, before.st_mtim), _ret);
	TEST_RES(timespec_equal(after.st_ctim, before.st_ctim), _ret);
}
END_TEST()

FN_TEST(tmpfile_invisible_in_readdir)
{
	DIR *dir;
	int fd;
	int found;
	struct dirent *entry;

	fd = TEST_SUCC(open(TEST_DIR, O_TMPFILE | O_RDWR, 0666));

	dir = TEST_RES(opendir(TEST_DIR), _ret != NULL);

	found = 0;
	while ((entry = readdir(dir)) != NULL) {
		if (strcmp(entry->d_name, ".") != 0 &&
		    strcmp(entry->d_name, "..") != 0) {
			found++;
		}
	}
	TEST_RES(found, found == 0);

	TEST_SUCC(closedir(dir));
	TEST_SUCC(close(fd));
}
END_TEST()

FN_TEST(tmpfile_write_and_linkat)
{
	DIR *dir;
	char buf[DATA_LEN];
	int dirfd;
	int fd;
	int found;
	int linked_fd;
	struct dirent *entry;
	struct stat stat_after_link;
	struct stat stat_after_second_link;

	fd = TEST_SUCC(open(TEST_DIR, O_TMPFILE | O_RDWR, 0666));

	TEST_RES(write(fd, DATA, DATA_LEN), _ret == DATA_LEN);

	TEST_RES(pread(fd, buf, sizeof(buf), 0), _ret == DATA_LEN);
	TEST_RES(memcmp(buf, DATA, DATA_LEN), _ret == 0);

	dirfd = TEST_SUCC(open(TEST_DIR, O_RDONLY | O_DIRECTORY));
	TEST_SUCC(linkat(fd, "", dirfd, LINKED_NAME, AT_EMPTY_PATH));

	TEST_RES(fstat(fd, &stat_after_link), stat_after_link.st_nlink == 1);

	dir = TEST_RES(opendir(TEST_DIR), _ret != NULL);
	found = 0;
	while ((entry = readdir(dir)) != NULL) {
		found += strcmp(entry->d_name, LINKED_NAME) == 0;
	}
	TEST_RES(found, found == 1);
	TEST_SUCC(closedir(dir));

	TEST_SUCC(link(TEST_DIR "/" LINKED_NAME,
		       TEST_DIR "/" LINKED_SECOND_NAME));
	TEST_RES(fstat(fd, &stat_after_second_link),
		 stat_after_second_link.st_nlink == 2);

	linked_fd = TEST_SUCC(open(TEST_DIR "/" LINKED_NAME, O_RDONLY));
	TEST_RES(pread(linked_fd, buf, sizeof(buf), 0), _ret == DATA_LEN);
	TEST_RES(memcmp(buf, DATA, DATA_LEN), _ret == 0);

	TEST_SUCC(close(linked_fd));
	TEST_SUCC(close(dirfd));
	TEST_SUCC(close(fd));
}
END_TEST()

FN_TEST(tmpfile_proc_fd_paths_follow_the_open_dentry)
{
	int fd = TEST_SUCC(open(TEST_DIR, O_TMPFILE | O_RDWR, 0600));
	struct stat metadata;
	TEST_SUCC(fstat(fd, &metadata));
	char anonymous_path[256];
	TEST_RES(snprintf(anonymous_path, sizeof(anonymous_path),
			  TEST_DIR "/#%ju (deleted)",
			  (uintmax_t)metadata.st_ino),
		 _ret > 0 && (size_t)_ret < sizeof(anonymous_path));
	TEST_RES(fd_link_matches(fd, anonymous_path), _ret == 1);

	int dirfd = TEST_SUCC(open(TEST_DIR, O_RDONLY | O_DIRECTORY));
	TEST_SUCC(linkat(fd, "", dirfd, PROC_LINKED_NAME, AT_EMPTY_PATH));
	TEST_RES(fd_link_matches(fd, anonymous_path), _ret == 1);
	int named_fd = TEST_SUCC(openat(dirfd, PROC_LINKED_NAME, O_RDONLY));
	TEST_RES(fd_link_matches(named_fd, TEST_DIR "/" PROC_LINKED_NAME),
		 _ret == 1);

	TEST_SUCC(unlinkat(dirfd, PROC_LINKED_NAME, 0));
	TEST_RES(fd_link_matches(fd, anonymous_path), _ret == 1);
	TEST_RES(fd_link_matches(named_fd,
				 TEST_DIR "/" PROC_LINKED_NAME " (deleted)"),
		 _ret == 1);
	TEST_SUCC(close(named_fd));
	TEST_SUCC(close(dirfd));
	TEST_SUCC(close(fd));
}
END_TEST()

FN_TEST(tmpfile_open_with_o_excl_cannot_be_linked)
{
	int fd;
	int dirfd;

	fd = TEST_SUCC(open(TEST_DIR, O_TMPFILE | O_RDWR | O_EXCL, 0666));
	dirfd = TEST_SUCC(open(TEST_DIR, O_RDONLY | O_DIRECTORY));

	TEST_ERRNO(linkat(fd, "", dirfd, LINKED_O_EXCL_NAME, AT_EMPTY_PATH),
		   ENOENT);

	TEST_SUCC(close(dirfd));
	TEST_SUCC(close(fd));
}
END_TEST()

FN_TEST(tmpfile_linkat_cross_mount_returns_exdev)
{
	int fd;
	int tmpfd;

	SKIP_TEST_IF(ext2_fixture_is_unavailable());

	fd = TEST_SUCC(open(TEST_DIR, O_TMPFILE | O_RDWR, 0666));
	tmpfd = TEST_SUCC(open(CROSS_LINK_DIR, O_RDONLY | O_DIRECTORY));

	TEST_ERRNO(linkat(fd, "", tmpfd, CROSS_LINK_NAME, AT_EMPTY_PATH),
		   EXDEV);

	TEST_SUCC(close(tmpfd));
	TEST_SUCC(close(fd));
}
END_TEST()

FN_SETUP(cleanup)
{
	cleanup_test_files();
}
END_SETUP()
