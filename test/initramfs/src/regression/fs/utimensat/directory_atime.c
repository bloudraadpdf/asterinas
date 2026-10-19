// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <dirent.h>
#include <fcntl.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <unistd.h>

#include "../../common/test.h"

FN_TEST(directory_noatime)
{
	char path[] = "/tmp/directory-atime-XXXXXX";
	TEST_RES(mkdtemp(path), _ret != NULL);
	const struct timespec times[] = { { 1, 0 }, { 2, 0 } };
	for (int mode = 0; mode < 5; ++mode) {
		TEST_SUCC(utimensat(AT_FDCWD, path, times, 0));
		struct stat before, after;
		TEST_SUCC(stat(path, &before));
		int flags = O_RDONLY | O_DIRECTORY;
		if (mode == 1 || mode == 4)
			flags |= O_NOATIME;
		int fd = TEST_SUCC(open(path, flags));
		if (mode == 2 || mode == 4) {
			flags = TEST_SUCC(fcntl(fd, F_GETFL));
			flags = mode == 2 ? flags | O_NOATIME :
					    flags & ~O_NOATIME;
			TEST_SUCC(fcntl(fd, F_SETFL, flags));
		}
		DIR *directory = TEST_RES(fdopendir(fd), _ret != NULL);
		errno = 0;
		while (readdir(directory)) {
		}
		int error = errno;
		TEST_RES(error, error == 0);
		TEST_SUCC(closedir(directory));
		TEST_SUCC(stat(path, &after));
		int same = before.st_atim.tv_sec == after.st_atim.tv_sec &&
			   before.st_atim.tv_nsec == after.st_atim.tv_nsec;
		TEST_RES(same, same == (mode == 1 || mode == 2));
	}
	TEST_SUCC(rmdir(path));
}
END_TEST()
