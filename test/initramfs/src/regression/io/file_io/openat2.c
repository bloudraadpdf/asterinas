// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE

#include <fcntl.h>
#include <linux/openat2.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <sys/mount.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#include "../../common/test.h"

#ifndef SYS_openat2
#define SYS_openat2 437
#endif

static char directory[] = "/tmp/openat2_XXXXXX";
static int basefd;
static int rootfd;
static struct stat inside;
static pthread_barrier_t race_start;
static atomic_bool stop_mounts;
static atomic_uint mount_rounds;
static char mount_source[256];
static char mount_target[256];

static int open_how(int dirfd, const char *path, uint64_t flags,
		    uint64_t resolve)
{
	struct open_how how = { .flags = flags, .resolve = resolve };
	return syscall(SYS_openat2, dirfd, path, &how, sizeof(how));
}

static void check_inside(int fd)
{
	struct stat statbuf = { 0 };
	CHECK(fd);
	CHECK(fstat(fd, &statbuf));
	CHECK_WITH(statbuf.st_ino, _ret == inside.st_ino);
	CHECK_WITH(statbuf.st_dev, _ret == inside.st_dev);
	CHECK(close(fd));
}

FN_SETUP(prepare)
{
	CHECK_WITH(mkdtemp(directory), _ret != NULL);
	basefd = CHECK(open(directory, O_RDONLY | O_DIRECTORY));
	CHECK(mkdirat(basefd, "root", 0700));
	rootfd = CHECK(openat(basefd, "root", O_RDONLY | O_DIRECTORY));
	int fd = CHECK(openat(rootfd, "file", O_CREAT | O_RDWR | O_EXCL, 0600));
	CHECK_WITH(write(fd, "keep", 4), _ret == 4);
	CHECK(fstat(fd, &inside));
	CHECK(close(fd));
	fd = CHECK(openat(basefd, "file", O_CREAT | O_RDWR | O_EXCL, 0600));
	CHECK(close(fd));
	CHECK(mkdirat(rootfd, "move", 0700));
	CHECK(mkdirat(rootfd, "move/sub", 0700));
	CHECK(mkdirat(rootfd, "mountpoint", 0700));
	CHECK(mkdirat(basefd, "mountsource", 0700));
	fd = CHECK(openat(rootfd, "mountpoint/file", O_CREAT | O_EXCL | O_RDWR,
			  0600));
	CHECK(close(fd));
	fd = CHECK(openat(basefd, "mountsource/file", O_CREAT | O_EXCL | O_RDWR,
			  0600));
	CHECK(close(fd));
	CHECK(symlinkat("file", rootfd, "relative"));
	CHECK(symlinkat("/file", rootfd, "absolute"));
}
END_SETUP()

FN_TEST(open_how_size_and_unknown_fields)
{
	struct {
		struct open_how how;
		uint64_t extension;
	} extended = { .how.flags = O_RDONLY };
	TEST_ERRNO(syscall(SYS_openat2, rootfd, "file", &extended, 0), EINVAL);
	TEST_ERRNO(syscall(SYS_openat2, rootfd, "file", &extended,
			   sizeof(struct open_how) - 1),
		   EINVAL);
	check_inside(TEST_SUCC(syscall(SYS_openat2, rootfd, "file", &extended,
				       sizeof(extended))));
	extended.extension = 1;
	TEST_ERRNO(syscall(SYS_openat2, rootfd, "file", &extended,
			   sizeof(extended)),
		   E2BIG);
	long page_size = CHECK(sysconf(_SC_PAGESIZE));
	TEST_ERRNO(syscall(SYS_openat2, rootfd, "file", NULL,
			   (size_t)page_size + 1),
		   E2BIG);
	TEST_ERRNO(syscall(SYS_openat2, rootfd, "file", NULL,
			   sizeof(struct open_how)),
		   EFAULT);
	TEST_ERRNO(open_how(rootfd, "file", 1ULL << 63, 0), EINVAL);
	TEST_ERRNO(open_how(rootfd, "file", 0, 1ULL << 63), EINVAL);
	TEST_ERRNO(open_how(rootfd, "file", 0,
			    RESOLVE_BENEATH | RESOLVE_IN_ROOT),
		   EINVAL);
	TEST_ERRNO(open_how(rootfd, "file", O_PATH | O_RDWR, 0), EINVAL);
	TEST_ERRNO(open_how(rootfd, "file", O_TMPFILE | O_CREAT | O_RDWR, 0),
		   EINVAL);
	struct open_how invalid_mode = { .mode = 0600 };
	TEST_ERRNO(syscall(SYS_openat2, rootfd, "file", &invalid_mode,
			   sizeof(invalid_mode)),
		   EINVAL);
}
END_TEST()

FN_TEST(scoped_roots_and_symlinks)
{
	check_inside(
		TEST_SUCC(open_how(rootfd, "relative", 0, RESOLVE_BENEATH)));
	TEST_ERRNO(open_how(rootfd, "absolute", 0, RESOLVE_BENEATH), EXDEV);
	TEST_ERRNO(open_how(rootfd, "/file", 0, RESOLVE_BENEATH), EXDEV);
	TEST_ERRNO(open_how(rootfd, "../file", 0, RESOLVE_BENEATH), EXDEV);
	check_inside(
		TEST_SUCC(open_how(rootfd, "absolute", 0, RESOLVE_IN_ROOT)));
	check_inside(TEST_SUCC(open_how(rootfd, "/file", 0, RESOLVE_IN_ROOT)));
	check_inside(
		TEST_SUCC(open_how(rootfd, "../file", 0, RESOLVE_IN_ROOT)));
	TEST_ERRNO(open_how(rootfd, "relative", 0, RESOLVE_NO_SYMLINKS), ELOOP);
	int fd = TEST_SUCC(open_how(rootfd, "relative", O_PATH | O_NOFOLLOW,
				    RESOLVE_NO_SYMLINKS));
	struct stat statbuf = { 0 };
	TEST_SUCC(fstat(fd, &statbuf));
	TEST_RES(S_ISLNK(statbuf.st_mode), _ret != 0);
	TEST_SUCC(close(fd));
	TEST_ERRNO(open_how(rootfd, "relative/", O_PATH | O_NOFOLLOW,
			    RESOLVE_NO_SYMLINKS),
		   ELOOP);
}
END_TEST()

static int child_error(const char *operation, int result, int expected_errno)
{
	int actual_errno = errno;
	if (result == -1 && actual_errno == expected_errno)
		return 1;
	fprintf(stderr, "openat2 %s: result=%d errno=%d expected=%d\n",
		operation, result, actual_errno, expected_errno);
	return 0;
}

FN_TEST(descriptor_limits_precede_filesystem_side_effects)
{
	pid_t child = CHECK(fork());
	if (child == 0) {
		struct rlimit limit;
		CHECK(getrlimit(RLIMIT_NOFILE, &limit));
		limit.rlim_cur = 0;
		CHECK(setrlimit(RLIMIT_NOFILE, &limit));
		struct open_how how = { .flags = O_CREAT | O_WRONLY,
					.mode = 0600 };
		int result = syscall(SYS_openat2, rootfd, "not-created", &how,
				     sizeof(how));
		int valid = child_error("create with no descriptors", result,
					EMFILE);
		how.flags = O_TRUNC | O_WRONLY;
		how.mode = 0;
		result =
			syscall(SYS_openat2, rootfd, "file", &how, sizeof(how));
		valid &= child_error("truncate with no descriptors", result,
				     EMFILE);
		result = open_how(rootfd, "", 0, 0);
		valid &= child_error("empty path with no descriptors", result,
				     EMFILE);
		result = open_how(rootfd, NULL, 0, 0);
		valid &= child_error("invalid path with no descriptors", result,
				     EMFILE);
		_exit(valid ? 0 : 1);
	}
	int status;
	TEST_RES(waitpid(child, &status, 0), _ret == child &&
						     WIFEXITED(status) &&
						     WEXITSTATUS(status) == 0);
	TEST_ERRNO(open_how(rootfd, "", 0, 0), ENOENT);
	TEST_ERRNO(open_how(rootfd, NULL, 0, 0), EFAULT);
	struct stat statbuf = { 0 };
	TEST_SUCC(fstatat(rootfd, "file", &statbuf, 0));
	TEST_RES(statbuf.st_size, _ret == 4);
	TEST_ERRNO(fstatat(rootfd, "not-created", &statbuf, 0), ENOENT);
	CHECK_WITH(unlinkat(rootfd, "not-created", 0),
		   _ret == 0 || errno == ENOENT);

	int available = CHECK(openat(rootfd, "file", O_RDONLY));
	CHECK(close(available));
	for (int attempt = 0; attempt < 32; ++attempt)
		TEST_ERRNO(open_how(rootfd, "missing", 0, 0), ENOENT);
	int fd = TEST_SUCC(open_how(rootfd, "file", O_CLOEXEC, 0));
	TEST_RES(fd, _ret == available);
	TEST_RES(fcntl(fd, F_GETFD), _ret == FD_CLOEXEC);
	TEST_SUCC(close(fd));
}
END_TEST()

FN_TEST(magic_links_and_mount_transitions)
{
	int file = CHECK(openat(rootfd, "file", O_RDONLY));
	char path[64];
	CHECK_WITH(snprintf(path, sizeof(path), "/proc/self/fd/%d", file),
		   _ret > 0 && (size_t)_ret < sizeof(path));
	TEST_ERRNO(open_how(AT_FDCWD, path, 0, RESOLVE_NO_MAGICLINKS), ELOOP);
	int fd = TEST_SUCC(open_how(AT_FDCWD, path, O_PATH | O_NOFOLLOW,
				    RESOLVE_NO_MAGICLINKS));
	TEST_SUCC(close(fd));
	TEST_SUCC(close(file));
	int procfd = CHECK(open("/proc/self/fd", O_PATH | O_DIRECTORY));
	CHECK_WITH(snprintf(path, sizeof(path), "%d", rootfd),
		   _ret > 0 && (size_t)_ret < sizeof(path));
	TEST_ERRNO(open_how(procfd, path, O_PATH, RESOLVE_NO_XDEV), EXDEV);
	TEST_ERRNO(open_how(procfd, path, O_PATH, RESOLVE_NO_MAGICLINKS),
		   ELOOP);
	TEST_SUCC(close(procfd));
	check_inside(TEST_SUCC(open_how(rootfd, "file", 0, RESOLVE_NO_XDEV)));
	TEST_ERRNO(open_how(AT_FDCWD, "/proc/self/stat", 0, RESOLVE_NO_XDEV),
		   EXDEV);
}
END_TEST()

FN_TEST(bind_mounts_are_distinct_mount_transitions)
{
	char source[256];
	char target[256];
	CHECK_WITH(snprintf(source, sizeof(source), "%s/mountsource",
			    directory),
		   _ret > 0 && (size_t)_ret < sizeof(source));
	CHECK_WITH(snprintf(target, sizeof(target), "%s/root/mountpoint",
			    directory),
		   _ret > 0 && (size_t)_ret < sizeof(target));
	int result = mount(source, target, NULL, MS_BIND, NULL);
	SKIP_TEST_IF(result == -1 && (errno == EPERM || errno == EACCES));
	CHECK(result);
	TEST_ERRNO(open_how(rootfd, "mountpoint", O_PATH, RESOLVE_NO_XDEV),
		   EXDEV);
	int mounted_fd =
		CHECK(openat(rootfd, "mountpoint", O_RDONLY | O_DIRECTORY));
	TEST_ERRNO(open_how(mounted_fd, "..", O_PATH, RESOLVE_NO_XDEV), EXDEV);
	TEST_SUCC(close(mounted_fd));
	TEST_SUCC(umount(target));
}
END_TEST()

static bool raced_lookup_stays_inside(int fd, const struct stat *expected)
{
	if (fd < 0)
		return errno == EAGAIN || errno == EXDEV || errno == ENOENT;
	struct stat actual;
	CHECK(fstat(fd, &actual));
	bool valid = actual.st_ino == expected->st_ino &&
		     actual.st_dev == expected->st_dev;
	CHECK(close(fd));
	return valid;
}

static void *change_mounts(void *unused)
{
	(void)unused;
	int ready = pthread_barrier_wait(&race_start);
	CHECK_WITH(ready, _ret == 0 || _ret == PTHREAD_BARRIER_SERIAL_THREAD);
	while (!atomic_load(&stop_mounts)) {
		CHECK(mount(mount_source, mount_target, NULL, MS_BIND, NULL));
		CHECK(umount2(mount_target, MNT_DETACH));
		atomic_fetch_add(&mount_rounds, 1);
	}
	return NULL;
}

FN_TEST(no_xdev_lookup_cannot_follow_a_racing_bind_mount)
{
	CHECK_WITH(snprintf(mount_source, sizeof(mount_source),
			    "%s/mountsource", directory),
		   _ret > 0 && (size_t)_ret < sizeof(mount_source));
	CHECK_WITH(snprintf(mount_target, sizeof(mount_target),
			    "%s/root/mountpoint", directory),
		   _ret > 0 && (size_t)_ret < sizeof(mount_target));
	int result = mount(mount_source, mount_target, NULL, MS_BIND, NULL);
	SKIP_TEST_IF(result == -1 && (errno == EPERM || errno == EACCES));
	CHECK(result);
	CHECK(umount2(mount_target, MNT_DETACH));
	struct stat expected;
	CHECK(fstatat(rootfd, "mountpoint/file", &expected, 0));
	CHECK_WITH(pthread_barrier_init(&race_start, NULL, 2), _ret == 0);
	pthread_t thread;
	CHECK_WITH(pthread_create(&thread, NULL, change_mounts, NULL),
		   _ret == 0);
	int ready = pthread_barrier_wait(&race_start);
	CHECK_WITH(ready, _ret == 0 || _ret == PTHREAD_BARRIER_SERIAL_THREAD);
	int valid = 1;
	for (int attempt = 0; attempt < 2000; ++attempt) {
		valid &= raced_lookup_stays_inside(
			open_how(rootfd, "mountpoint/file", O_RDONLY,
				 RESOLVE_NO_XDEV),
			&expected);
		if (attempt % 16 == 0)
			sched_yield();
	}
	atomic_store(&stop_mounts, true);
	CHECK_WITH(pthread_join(thread, NULL), _ret == 0);
	CHECK_WITH(pthread_barrier_destroy(&race_start), _ret == 0);
	TEST_RES(valid, _ret == 1);
	TEST_RES(atomic_load(&mount_rounds), _ret > 0);
}
END_TEST()

FN_TEST(cached_lookup_has_hits_misses_and_negative_entries)
{
	check_inside(TEST_SUCC(open_how(rootfd, "file", 0, RESOLVE_CACHED)));
	TEST_ERRNO(open_how(rootfd, "cold", 0, RESOLVE_CACHED), EAGAIN);
	TEST_ERRNO(openat(rootfd, "cold", O_RDONLY), ENOENT);
	TEST_ERRNO(open_how(rootfd, "cold", 0, RESOLVE_CACHED), ENOENT);
	TEST_ERRNO(open_how(rootfd, "file", O_CREAT, RESOLVE_CACHED), EAGAIN);
	TEST_ERRNO(open_how(rootfd, "file", O_TRUNC | O_WRONLY, RESOLVE_CACHED),
		   EAGAIN);
	TEST_ERRNO(open_how(rootfd, ".", O_TMPFILE | O_RDWR, RESOLVE_CACHED),
		   EAGAIN);
}
END_TEST()

FN_TEST(cached_symlink_on_resident_noatime_filesystem)
{
	char target[256];
	CHECK(mkdirat(basefd, "cached", 0700));
	CHECK_WITH(snprintf(target, sizeof(target), "%s/cached", directory),
		   _ret > 0 && (size_t)_ret < sizeof(target));
	int result = mount("tmpfs", target, "tmpfs", MS_NOATIME, NULL);
	if (result == -1 && (errno == EPERM || errno == EACCES)) {
		CHECK(unlinkat(basefd, "cached", AT_REMOVEDIR));
		SKIP_TEST_IF(1);
	}
	CHECK(result);
	int dirfd = CHECK(openat(basefd, "cached", O_RDONLY | O_DIRECTORY));
	int file =
		CHECK(openat(dirfd, "file", O_CREAT | O_EXCL | O_RDWR, 0600));
	struct stat expected;
	CHECK(fstat(file, &expected));
	CHECK(close(file));
	CHECK(symlinkat("file", dirfd, "link"));
	file = CHECK(open_how(dirfd, "link", 0, 0));
	CHECK(close(file));
	/* No atime update or filesystem fetch may force this resident link out
	 * of Linux's RCU path walk. */
	file = TEST_SUCC(open_how(dirfd, "link", 0, RESOLVE_CACHED));
	if (file >= 0) {
		struct stat actual;
		TEST_SUCC(fstat(file, &actual));
		TEST_RES(actual.st_ino, _ret == expected.st_ino);
		TEST_RES(actual.st_dev, _ret == expected.st_dev);
		TEST_SUCC(close(file));
	}
	CHECK(unlinkat(dirfd, "link", 0));
	CHECK(unlinkat(dirfd, "file", 0));
	CHECK(close(dirfd));
	CHECK(umount(target));
	CHECK(unlinkat(basefd, "cached", AT_REMOVEDIR));
}
END_TEST()

FN_TEST(ioctl_only_access_mode_preserves_permissions_and_disables_io)
{
	int fd = TEST_SUCC(open_how(rootfd, "file", O_ACCMODE, 0));
	TEST_RES(fcntl(fd, F_GETFL) & O_ACCMODE, _ret == O_ACCMODE);
	char byte;
	TEST_ERRNO(read(fd, &byte, 1), EBADF);
	TEST_ERRNO(write(fd, "x", 1), EBADF);
	TEST_SUCC(close(fd));
	TEST_ERRNO(open_how(rootfd, ".", O_ACCMODE, 0), EISDIR);
	CHECK(fchmodat(rootfd, "file", 0400, 0));
	if (geteuid() != 0)
		TEST_ERRNO(open_how(rootfd, "file", O_ACCMODE, 0), EACCES);
	CHECK(fchmodat(rootfd, "file", 0600, 0));
	CHECK(mkfifoat(rootfd, "fifo", 0600));
	TEST_ERRNO(open_how(rootfd, "fifo", O_ACCMODE | O_NONBLOCK, 0), EINVAL);
	TEST_SUCC(unlinkat(rootfd, "fifo", 0));
}
END_TEST()

static atomic_bool stop_renames;
static atomic_uint rename_rounds;

static void *move_directory(void *unused)
{
	(void)unused;
	int ready = pthread_barrier_wait(&race_start);
	CHECK_WITH(ready, _ret == 0 || _ret == PTHREAD_BARRIER_SERIAL_THREAD);
	while (!atomic_load(&stop_renames)) {
		CHECK(renameat(rootfd, "move", basefd, "away"));
		CHECK(renameat(basefd, "away", rootfd, "move"));
		atomic_fetch_add(&rename_rounds, 1);
	}
	return NULL;
}

FN_TEST(scoped_lookup_cannot_escape_during_directory_rename)
{
	CHECK_WITH(pthread_barrier_init(&race_start, NULL, 2), _ret == 0);
	pthread_t thread;
	CHECK_WITH(pthread_create(&thread, NULL, move_directory, NULL),
		   _ret == 0);
	int ready = pthread_barrier_wait(&race_start);
	CHECK_WITH(ready, _ret == 0 || _ret == PTHREAD_BARRIER_SERIAL_THREAD);
	int valid = 1;
	uint64_t scopes[] = { RESOLVE_BENEATH, RESOLVE_IN_ROOT };
	for (size_t i = 0; i < sizeof(scopes) / sizeof(scopes[0]); ++i) {
		for (int attempt = 0; attempt < 2000; ++attempt) {
			valid &= raced_lookup_stays_inside(
				open_how(rootfd, "move/sub/../../file", 0,
					 scopes[i]),
				&inside);
			if (attempt % 16 == 0)
				sched_yield();
		}
	}
	atomic_store(&stop_renames, true);
	CHECK_WITH(pthread_join(thread, NULL), _ret == 0);
	CHECK_WITH(pthread_barrier_destroy(&race_start), _ret == 0);
	TEST_RES(valid, _ret == 1);
	TEST_RES(atomic_load(&rename_rounds), _ret > 0);
}
END_TEST()

FN_SETUP(cleanup)
{
	CHECK(unlinkat(rootfd, "absolute", 0));
	CHECK(unlinkat(rootfd, "relative", 0));
	CHECK(unlinkat(rootfd, "file", 0));
	CHECK(unlinkat(rootfd, "move/sub", AT_REMOVEDIR));
	CHECK(unlinkat(rootfd, "move", AT_REMOVEDIR));
	CHECK(unlinkat(rootfd, "mountpoint/file", 0));
	CHECK(unlinkat(rootfd, "mountpoint", AT_REMOVEDIR));
	CHECK(close(rootfd));
	CHECK(unlinkat(basefd, "root", AT_REMOVEDIR));
	CHECK(unlinkat(basefd, "mountsource/file", 0));
	CHECK(unlinkat(basefd, "mountsource", AT_REMOVEDIR));
	CHECK(unlinkat(basefd, "file", 0));
	CHECK(close(basefd));
	CHECK(rmdir(directory));
}
END_SETUP()
