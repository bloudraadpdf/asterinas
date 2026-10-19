// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <signal.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <sys/uio.h>
#include <unistd.h>

#include "../common/test.h"

static volatile sig_atomic_t interrupted;

static void on_alarm(int signal)
{
	interrupted = signal;
}

FN_TEST(timeout_values)
{
	int pair[2];
	TEST_SUCC(socketpair(AF_UNIX, SOCK_STREAM, 0, pair));
	struct timeval value = { 0 };
	TEST_ERRNO(setsockopt(pair[0], SOL_SOCKET, SO_RCVTIMEO, (void *)1, 3),
		   EINVAL);
	TEST_ERRNO(setsockopt(pair[0], SOL_SOCKET, SO_RCVTIMEO, (void *)1, 4),
		   EFAULT);
	value.tv_usec = -1;
	TEST_ERRNO(setsockopt(pair[0], SOL_SOCKET, SO_RCVTIMEO, &value,
			      sizeof(value)),
		   EDOM);
	value = (struct timeval){ .tv_sec = LONG_MAX };
	TEST_SUCC(setsockopt(pair[0], SOL_SOCKET, SO_RCVTIMEO, &value,
			     sizeof(value)));
	socklen_t length = sizeof(value);
	TEST_SUCC(
		getsockopt(pair[0], SOL_SOCKET, SO_RCVTIMEO, &value, &length));
	TEST_RES(length, length == sizeof(value));
	TEST_RES(value.tv_sec, value.tv_sec == 0 && value.tv_usec == 0);
	value.tv_sec = -1;
	TEST_SUCC(setsockopt(pair[0], SOL_SOCKET, SO_RCVTIMEO, &value,
			     sizeof(value)));
	char byte;
	TEST_ERRNO(read(pair[0], &byte, 1), EAGAIN);
	TEST_SUCC(close(pair[0]));
	TEST_SUCC(close(pair[1]));
}
END_TEST()

FN_TEST(nonblocking_is_per_call)
{
	const int types[] = { SOCK_STREAM, SOCK_DGRAM, SOCK_SEQPACKET };
	for (unsigned i = 0; i < sizeof(types) / sizeof(*types); ++i) {
		int pair[2];
		char byte;
		TEST_SUCC(socketpair(AF_UNIX, types[i], 0, pair));
		TEST_ERRNO(recv(pair[0], &byte, 1, MSG_DONTWAIT), EAGAIN);
		TEST_RES(fcntl(pair[0], F_GETFL), (_ret & O_NONBLOCK) == 0);
		TEST_SUCC(close(pair[0]));
		TEST_SUCC(close(pair[1]));
	}
}
END_TEST()

FN_TEST(timed_receive_is_not_restarted)
{
	struct sigaction action = { .sa_handler = on_alarm,
				    .sa_flags = SA_RESTART };
	TEST_SUCC(sigemptyset(&action.sa_mask));
	TEST_SUCC(sigaction(SIGALRM, &action, NULL));
	for (int operation = 0; operation < 4; ++operation) {
		int pair[2];
		char byte;
		struct iovec vector = { .iov_base = &byte, .iov_len = 1 };
		struct msghdr message = { .msg_iov = &vector, .msg_iovlen = 1 };
		TEST_SUCC(socketpair(AF_UNIX, SOCK_STREAM, 0, pair));
		struct timeval timeout = { .tv_sec = 1 };
		TEST_SUCC(setsockopt(pair[0], SOL_SOCKET, SO_RCVTIMEO, &timeout,
				     sizeof(timeout)));
		struct itimerval timer = { .it_value = { .tv_usec = 20000 } };
		interrupted = 0;
		TEST_SUCC(setitimer(ITIMER_REAL, &timer, NULL));
		ssize_t result;
		if (operation == 0)
			result = read(pair[0], &byte, 1);
		else if (operation == 1)
			result = readv(pair[0], &vector, 1);
		else if (operation == 2)
			result = recvfrom(pair[0], &byte, 1, 0, NULL, NULL);
		else
			result = recvmsg(pair[0], &message, 0);
		int error = errno;
		TEST_RES(result, result == -1 && error == EINTR);
		TEST_RES(interrupted, interrupted == SIGALRM);
		TEST_SUCC(close(pair[0]));
		TEST_SUCC(close(pair[1]));
	}
}
END_TEST()
