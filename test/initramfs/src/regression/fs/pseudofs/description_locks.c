// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/file.h>
#include <sys/signalfd.h>
#include <sys/timerfd.h>
#include <sys/wait.h>
#include <unistd.h>

static struct flock range(short type)
{
	return (struct flock){
		.l_type = type, .l_whence = SEEK_SET, .l_start = 7, .l_len = 3
	};
}

static void anonymous_locks(void)
{
	sigset_t mask;
	assert(sigemptyset(&mask) == 0);
	int fd[] = { eventfd(0, 0), eventfd(0, 0), epoll_create1(0),
		     timerfd_create(CLOCK_MONOTONIC, 0),
		     signalfd(-1, &mask, 0) };
	for (unsigned i = 0; i < sizeof(fd) / sizeof(fd[0]); ++i)
		assert(fd[i] >= 0);
	assert(flock(fd[0], LOCK_EX | LOCK_NB) == 0);
	for (unsigned i = 1; i < sizeof(fd) / sizeof(fd[0]); ++i) {
		errno = 0;
		assert(flock(fd[i], LOCK_EX | LOCK_NB) == -1 &&
		       errno == EWOULDBLOCK);
	}
	assert(flock(fd[0], LOCK_UN) == 0);
	assert(flock(fd[0], LOCK_SH) == 0);
	assert(flock(fd[1], LOCK_SH) == 0);
	errno = 0;
	assert(flock(fd[0], LOCK_EX | LOCK_NB) == -1 && errno == EWOULDBLOCK);
	assert(flock(fd[1], LOCK_UN) == 0);
	assert(flock(fd[1], LOCK_EX | LOCK_NB) == 0);
	assert(flock(fd[1], LOCK_UN) == 0);

	struct flock lock = range(F_WRLCK);
	assert(fcntl(fd[0], F_OFD_SETLK, &lock) == 0);
	struct flock query = lock;
	assert(fcntl(fd[1], F_OFD_GETLK, &query) == 0);
	assert(query.l_type == F_WRLCK && query.l_pid == -1 &&
	       query.l_start == 7 && query.l_len == 3);
	query = lock;
	query.l_pid = 1;
	errno = 0;
	assert(fcntl(fd[1], F_OFD_GETLK, &query) == -1 && errno == EINVAL);
	int duplicate = dup(fd[0]);
	assert(duplicate >= 0 && close(fd[0]) == 0);
	errno = 0;
	assert(fcntl(fd[1], F_OFD_SETLK, &lock) == -1 && errno == EAGAIN);
	assert(close(duplicate) == 0);
	assert(fcntl(fd[1], F_OFD_SETLK, &lock) == 0);
	for (unsigned i = 1; i < sizeof(fd) / sizeof(fd[0]); ++i)
		assert(close(fd[i]) == 0);
}

static void regular_locks(void)
{
	const char *name = "/tmp/description-lock-regression";
	int first = open(name, O_CREAT | O_TRUNC | O_RDWR, 0600);
	int second = open(name, O_RDWR);
	assert(first >= 0 && second >= 0 && unlink(name) == 0);
	struct flock lock = range(F_WRLCK);
	assert(fcntl(first, F_OFD_SETLK, &lock) == 0);
	errno = 0;
	assert(fcntl(first, F_SETLK, &lock) == -1 && errno == EAGAIN);
	struct flock query = lock;
	assert(fcntl(first, F_OFD_GETLK, &query) == 0 &&
	       query.l_type == F_UNLCK);
	lock = range(F_UNLCK);
	assert(fcntl(first, F_OFD_SETLK, &lock) == 0);
	lock = range(F_WRLCK);
	assert(fcntl(first, F_SETLK, &lock) == 0);
	query = lock;
	assert(fcntl(second, F_OFD_GETLK, &query) == 0);
	assert(query.l_type == F_WRLCK && query.l_pid == getpid());
	assert(close(second) == 0);
	assert(fcntl(first, F_OFD_SETLK, &lock) == 0);

	int sync[2];
	assert(pipe(sync) == 0);
	pid_t child = fork();
	assert(child >= 0);
	if (child == 0) {
		assert(close(sync[0]) == 0);
		assert(close(first) == 0);
		assert(write(sync[1], "x", 1) == 1);
		_exit(0);
	}
	assert(close(sync[1]) == 0);
	char ready;
	assert(read(sync[0], &ready, 1) == 1 && close(sync[0]) == 0);
	query = range(F_WRLCK);
	assert(fcntl(first, F_GETLK, &query) == 0 && query.l_type == F_WRLCK &&
	       query.l_pid == -1);
	int status;
	assert(waitpid(child, &status, 0) == child && WIFEXITED(status) &&
	       WEXITSTATUS(status) == 0);
	assert(close(first) == 0);
}

int main(void)
{
	anonymous_locks();
	regular_locks();
	return 0;
}
