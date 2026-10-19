/* SPDX-License-Identifier: MPL-2.0 */
#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <unistd.h>

int main(void)
{
	struct rlimit original, limit;
	assert(getrlimit(RLIMIT_NOFILE, &original) == 0);
	int retained = fcntl(STDOUT_FILENO, F_DUPFD, 63);
	assert(retained == 63);
	limit = original;
	limit.rlim_cur = 10;
	assert(setrlimit(RLIMIT_NOFILE, &limit) == 0);
	assert(fcntl(retained, F_GETFD) >= 0);
	assert(dup2(retained, retained) == retained);
	errno = 0;
	assert(fcntl(STDOUT_FILENO, F_DUPFD, 10) == -1 && errno == EINVAL);
	errno = 0;
	assert(fcntl(-1, F_DUPFD, -1) == -1 && errno == EBADF);
	int opened[10], count = 0, fd;
	while ((fd = dup(STDOUT_FILENO)) >= 0) {
		assert(count < 10 && fd < 10);
		opened[count++] = fd;
	}
	assert(errno == EMFILE && count > 0 && opened[count - 1] == 9);
	assert(close(opened[--count]) == 0);
	int pair[2];
	errno = 0;
	assert(pipe(pair) == -1 && errno == EMFILE);
	errno = 0;
	assert(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == -1 &&
	       errno == EMFILE);
	fd = open("/dev/null", O_RDONLY);
	assert(fd == 9);
	assert(close(fd) == 0);
	limit.rlim_cur = 0;
	assert(setrlimit(RLIMIT_NOFILE, &limit) == 0);
	errno = 0;
	assert(dup(STDOUT_FILENO) == -1 && errno == EMFILE);
	errno = 0;
	assert(fcntl(STDOUT_FILENO, F_DUPFD, 0) == -1 && errno == EINVAL);
	assert(setrlimit(RLIMIT_NOFILE, &original) == 0);
	while (count)
		assert(close(opened[--count]) == 0);
	assert(close(retained) == 0);
	return 0;
}
