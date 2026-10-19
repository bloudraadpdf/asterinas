// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <unistd.h>
#include <sys/stat.h>

#include "../../common/test.h"

static int reopen_pipe(int fd, int flags)
{
	char path[64];
	snprintf(path, sizeof(path), "/proc/self/fd/%d", fd);
	return open(path, flags);
}

FN_TEST(pipe_reopen_clears_hangup)
{
	int pair[2] = { -1, -1 };
	TEST_SUCC(pipe2(pair, O_NONBLOCK));
	int retained = TEST_SUCC(reopen_pipe(pair[0], O_PATH));
	TEST_SUCC(close(pair[1]));
	struct pollfd watch = { .fd = pair[0], .events = POLLIN | POLLOUT };
	TEST_RES(poll(&watch, 1, 0), _ret == 1 && watch.revents == POLLHUP);
	int writer = TEST_SUCC(reopen_pipe(retained, O_WRONLY | O_NONBLOCK));
	TEST_RES(poll(&watch, 1, 0), _ret == 0);
	TEST_SUCC(close(pair[0]));
	watch.fd = writer;
	TEST_RES(poll(&watch, 1, 0),
		 _ret == 1 && (watch.revents & (POLLERR | POLLHUP)) == POLLERR);
	int reader = TEST_SUCC(reopen_pipe(retained, O_RDONLY | O_NONBLOCK));
	TEST_RES(poll(&watch, 1, 0), _ret == 1 && watch.revents == POLLOUT);
	TEST_RES(write(writer, "a", 1), _ret == 1);
	char byte;
	TEST_RES(read(reader, &byte, 1), _ret == 1 && byte == 'a');
	TEST_SUCC(close(writer));
	TEST_SUCC(close(reader));
	reader = TEST_SUCC(reopen_pipe(retained, O_RDONLY | O_NONBLOCK));
	TEST_RES(read(reader, &byte, 1), _ret == 0);
	TEST_SUCC(close(reader));
	TEST_SUCC(close(retained));
}
END_TEST()

FN_TEST(pipe_last_close_discards_data)
{
	for (int packet = 0; packet < 2; ++packet) {
		int pair[2] = { -1, -1 };
		TEST_SUCC(pipe2(pair, O_NONBLOCK | (packet ? O_DIRECT : 0)));
		int retained = TEST_SUCC(reopen_pipe(pair[0], O_PATH));
		int duplicate = TEST_SUCC(dup(pair[0]));
		TEST_RES(write(pair[1], "ab", 2), _ret == 2);
		TEST_SUCC(close(pair[0]));
		TEST_SUCC(close(pair[1]));
		char bytes[2];
		TEST_RES(read(duplicate, bytes, 1),
			 _ret == 1 && bytes[0] == 'a');
		TEST_SUCC(close(duplicate));
		int reader =
			TEST_SUCC(reopen_pipe(retained, O_RDONLY | O_NONBLOCK));
		TEST_RES(read(reader, bytes, 2), _ret == 0);
		TEST_SUCC(close(reader));
		TEST_SUCC(close(retained));
	}
}
END_TEST()

FN_TEST(packet_pipe_has_bounded_slots)
{
	int pair[2] = { -1, -1 };
	TEST_SUCC(pipe2(pair, O_NONBLOCK | O_DIRECT));
	int count = 0;
	while (count < 1024 && write(pair[1], "ab", 2) == 2) {
		TEST_RES(++count, _ret < 1024);
	}
	int stopped_errno = errno;
	TEST_RES(stopped_errno, _ret == EAGAIN);
	struct pollfd watch = { .fd = pair[1], .events = POLLOUT };
	TEST_RES(poll(&watch, 1, 0), _ret == 0);
	char bytes[4];
	TEST_RES(read(pair[0], bytes, 1), _ret == 1 && bytes[0] == 'a');
	TEST_RES(poll(&watch, 1, 0), _ret == 1 && watch.revents == POLLOUT);
	TEST_RES(write(pair[1], "cd", 2), _ret == 2);
	for (int i = 0; i < count - 1; ++i)
		TEST_RES(read(pair[0], bytes, sizeof(bytes)),
			 _ret == 2 && bytes[0] == 'a' && bytes[1] == 'b');
	TEST_RES(read(pair[0], bytes, sizeof(bytes)),
		 _ret == 2 && bytes[0] == 'c' && bytes[1] == 'd');
	TEST_ERRNO(read(pair[0], bytes, 1), EAGAIN);
	TEST_SUCC(close(pair[0]));
	TEST_RES(write(pair[1], "", 0), _ret == 0);
	TEST_SUCC(close(pair[1]));
}
END_TEST()

FN_TEST(named_pipe_hangup_needs_a_writer)
{
	char path[64];
	snprintf(path, sizeof(path), "/tmp/fifo-lifetime-%d", getpid());
	TEST_SUCC(mkfifo(path, 0600));
	int reader = TEST_SUCC(open(path, O_RDONLY | O_NONBLOCK));
	struct pollfd watch = { .fd = reader, .events = POLLIN };
	TEST_RES(poll(&watch, 1, 0), _ret == 0);
	char byte;
	TEST_RES(read(reader, &byte, 1), _ret == 0);
	int writer = TEST_SUCC(open(path, O_WRONLY | O_NONBLOCK));
	TEST_RES(poll(&watch, 1, 0), _ret == 0);
	TEST_SUCC(close(writer));
	TEST_RES(poll(&watch, 1, 0), _ret == 1 && watch.revents == POLLHUP);
	int fresh = TEST_SUCC(open(path, O_RDONLY | O_NONBLOCK));
	watch.fd = fresh;
	TEST_RES(poll(&watch, 1, 0), _ret == 0);
	watch.fd = reader;
	TEST_RES(poll(&watch, 1, 0), _ret == 1 && watch.revents == POLLHUP);
	watch.fd = fresh;
	TEST_RES(poll(&watch, 1, 0), _ret == 0);
	TEST_SUCC(close(reader));
	TEST_SUCC(close(fresh));
	TEST_SUCC(unlink(path));
}
END_TEST()
