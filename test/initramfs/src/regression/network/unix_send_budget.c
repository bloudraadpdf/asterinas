// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <poll.h>
#include <stdio.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

static void check_packets(int type)
{
	int pair[2], size = 4096;
	assert(socketpair(AF_UNIX, type, 0, pair) == 0);
	assert(setsockopt(pair[0], SOL_SOCKET, SO_SNDBUF, &size,
			  sizeof(size)) == 0);
	socklen_t length = sizeof(size);
	assert(getsockopt(pair[0], SOL_SOCKET, SO_SNDBUF, &size, &length) == 0);
	assert(size == 8192);
	unsigned packets = 0;
	while (send(pair[0], "", 0, MSG_DONTWAIT | MSG_NOSIGNAL) == 0)
		assert(++packets < 1000);
	assert(errno == EAGAIN && packets > 0);
	char byte;
	assert(recv(pair[1], &byte, 1, MSG_PEEK | MSG_DONTWAIT) == 0);
	errno = 0;
	assert(send(pair[0], "", 0, MSG_DONTWAIT | MSG_NOSIGNAL) == -1);
	assert(errno == EAGAIN);
	for (unsigned index = 0; index < packets; ++index)
		assert(recv(pair[1], &byte, 1, MSG_DONTWAIT) == 0);
	struct pollfd ready = { .fd = pair[0], .events = POLLOUT };
	assert(poll(&ready, 1, 0) == 1 && (ready.revents & POLLOUT));
	assert(send(pair[0], "a", 1, MSG_DONTWAIT | MSG_NOSIGNAL) == 1);
	assert(recv(pair[1], &byte, 1, MSG_DONTWAIT) == 1 && byte == 'a');
	assert(close(pair[0]) == 0);
	assert(close(pair[1]) == 0);
}

static void check_stream_timeout(void)
{
	int pair[2], size = 4096;
	assert(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0);
	assert(setsockopt(pair[0], SOL_SOCKET, SO_SNDBUF, &size,
			  sizeof(size)) == 0);
	struct timeval timeout = { .tv_usec = 40000 };
	assert(setsockopt(pair[0], SOL_SOCKET, SO_SNDTIMEO, &timeout,
			  sizeof(timeout)) == 0);
	char data[65536] = { 0 };
	struct timespec before, after;
	assert(clock_gettime(CLOCK_MONOTONIC, &before) == 0);
	ssize_t sent = send(pair[0], data, sizeof(data), MSG_NOSIGNAL);
	assert(clock_gettime(CLOCK_MONOTONIC, &after) == 0);
	assert(sent > 0 && (size_t)sent < sizeof(data));
	long long elapsed = (after.tv_sec - before.tv_sec) * 1000000000LL +
			    after.tv_nsec - before.tv_nsec;
	assert(elapsed >= 20000000 && elapsed < 2000000000);
	assert(close(pair[0]) == 0);
	assert(close(pair[1]) == 0);
}

static void check_packet_allocation(int type)
{
	int pair[2], size = 1;
	char bytes[4577] = { 0 };
	assert(socketpair(AF_UNIX, type | SOCK_NONBLOCK, 0, pair) == 0);
	assert(setsockopt(pair[0], SOL_SOCKET, SO_SNDBUF, &size,
			  sizeof(size)) == 0);
	assert(send(pair[0], bytes, sizeof(bytes), MSG_NOSIGNAL) == -1 &&
	       errno == EMSGSIZE);
	assert(send(pair[0], bytes, 4000, MSG_NOSIGNAL) == 4000);
	assert(send(pair[0], bytes, 4000, MSG_NOSIGNAL) == -1 &&
	       errno == EAGAIN);
	struct pollfd ready = { .fd = pair[0], .events = POLLOUT };
	assert(poll(&ready, 1, 0) == 0);
	assert(recv(pair[1], bytes, 1, MSG_PEEK) == 1);
	assert(send(pair[0], bytes, 4000, MSG_NOSIGNAL) == -1 &&
	       errno == EAGAIN);
	assert(recv(pair[1], bytes, 1, 0) == 1);
	assert(poll(&ready, 1, 0) == 1 && (ready.revents & POLLOUT));
	assert(send(pair[0], bytes, 4576, MSG_NOSIGNAL) == 4576);
	assert(recv(pair[1], bytes, sizeof(bytes), 0) == 4576);
	assert(close(pair[0]) == 0);
	assert(close(pair[1]) == 0);
}

static void check_datagram_peer_close(void)
{
	int pair[2], error = -1;
	socklen_t length = sizeof(error);
	assert(socketpair(AF_UNIX, SOCK_DGRAM | SOCK_NONBLOCK, 0, pair) == 0);
	assert(send(pair[0], "a", 1, MSG_NOSIGNAL) == 1);
	assert(close(pair[1]) == 0);
	assert(getsockopt(pair[0], SOL_SOCKET, SO_ERROR, &error, &length) == 0);
	assert(error == 0 && length == sizeof(error));
	assert(send(pair[0], "b", 1, MSG_NOSIGNAL) == -1 &&
	       errno == ECONNREFUSED);
	assert(send(pair[0], "c", 1, MSG_NOSIGNAL) == -1 && errno == ENOTCONN);
	assert(close(pair[0]) == 0);
}

static void check_stream_eof(void)
{
	int pair[2];
	assert(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0);
	char bytes[2];
	errno = 0;
	assert(recv(pair[1], bytes, 0, MSG_DONTWAIT) == -1 && errno == EAGAIN);
	assert(send(pair[0], "a", 1, MSG_NOSIGNAL) == 1);
	assert(recv(pair[1], bytes, 0, MSG_DONTWAIT) == 0);
	assert(close(pair[0]) == 0);
	assert(recv(pair[1], bytes, sizeof(bytes), 0) == 1 && bytes[0] == 'a');
	assert(recv(pair[1], bytes, sizeof(bytes), 0) == 0);
	assert(recv(pair[1], bytes, 0, MSG_DONTWAIT) == 0);
	assert(close(pair[1]) == 0);
}

int main(void)
{
	check_packets(SOCK_DGRAM);
	check_packets(SOCK_SEQPACKET);
	check_packet_allocation(SOCK_DGRAM);
	check_packet_allocation(SOCK_SEQPACKET);
	check_datagram_peer_close();
	check_stream_timeout();
	check_stream_eof();
	puts("PASS bounded empty messages, peek accounting and stream timeout "
	     "progress");
	return 0;
}
