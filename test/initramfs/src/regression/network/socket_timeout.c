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
