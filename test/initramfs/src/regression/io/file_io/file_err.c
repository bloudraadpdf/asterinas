// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE

#include "../../common/test.h"
#include <stdint.h>
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

static int fd;

FN_SETUP(open)
{
	fd = CHECK(open("/etc/passwd", O_RDONLY));
}
END_SETUP()

FN_TEST(dup_out_of_range)
{
	// `-1` is out of the allowed range for file descriptors.
	TEST_ERRNO(dup2(fd, -1), EBADF);
}
END_TEST()

FN_TEST(dup_invalid_old_preserves_new)
{
	char buf[1];
	int new_fd = CHECK(dup(fd));

	TEST_ERRNO(dup2(-1, new_fd), EBADF);
	TEST_RES(read(new_fd, buf, sizeof(buf)), _ret == 1);

	TEST_ERRNO(syscall(SYS_dup3, -1, new_fd, 0), EBADF);
	TEST_RES(read(new_fd, buf, sizeof(buf)), _ret == 1);

	TEST_SUCC(close(new_fd));
}
END_TEST()

FN_TEST(flock_overflow)
{
	struct flock fl;

	fl.l_type = F_RDLCK;
	fl.l_whence = SEEK_SET;

	// `l_start + l_len` underflows.
	fl.l_start = -1;
	fl.l_len = INT64_MIN;
	TEST_ERRNO(fcntl(fd, F_SETLK, &fl), EINVAL);

	// `l_start + l_len` overflows.
	fl.l_start = 2;
	fl.l_len = INT64_MAX;
	TEST_ERRNO(fcntl(fd, F_SETLK, &fl), EOVERFLOW);
}
END_TEST()

FN_TEST(ftruncate_large)
{
	int memfd;

	memfd = TEST_SUCC(memfd_create("test_memfd", 0));

	// `ftruncate` can handle large expansions and shrinking.
	TEST_SUCC(ftruncate(memfd, ((off_t)1) << 50));
	TEST_SUCC(ftruncate(memfd, 0));

	TEST_SUCC(close(memfd));
}
END_TEST()

FN_TEST(ftruncate_append_descriptor)
{
	int modes[] = { O_WRONLY, O_RDWR };
	for (size_t i = 0; i < sizeof(modes) / sizeof(modes[0]); ++i) {
		char path[] = "/tmp/ftruncate_append_XXXXXX";
		int temporary_fd = CHECK(mkstemp(path));
		int append_fd = CHECK(open(path, modes[i] | O_APPEND));
		CHECK(close(temporary_fd));
		CHECK(unlink(path));

		TEST_RES(write(append_fd, "abc", 3), _ret == 3);
		TEST_RES(lseek(append_fd, 1, SEEK_SET), _ret == 1);
		TEST_SUCC(ftruncate(append_fd, 8));
		struct stat statbuf = { 0 };
		TEST_SUCC(fstat(append_fd, &statbuf));
		TEST_RES(statbuf.st_size, _ret == 8);
		TEST_SUCC(ftruncate(append_fd, 2));
		TEST_RES(lseek(append_fd, 0, SEEK_CUR), _ret == 1);
		TEST_RES(write(append_fd, "d", 1), _ret == 1);
		TEST_SUCC(fstat(append_fd, &statbuf));
		TEST_RES(statbuf.st_size, _ret == 3);
		TEST_RES(lseek(append_fd, 0, SEEK_CUR), _ret == 3);
		TEST_SUCC(close(append_fd));
	}
}
END_TEST()

FN_SETUP(cleanup)
{
	CHECK(close(fd));
}
END_SETUP()
