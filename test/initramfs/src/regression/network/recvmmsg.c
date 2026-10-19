// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <poll.h>
#include <stdio.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static void check_batch(int type)
{
	int pair[2];
	assert(socketpair(AF_UNIX, type, 0, pair) == 0);
	char bytes[2] = { 0 };
	struct iovec vectors[2] = { { bytes, 1 }, { bytes + 1, 1 } };
	struct mmsghdr messages[2] = {
		{ .msg_hdr = { .msg_iov = &vectors[0], .msg_iovlen = 1 } },
		{ .msg_hdr = { .msg_iov = &vectors[1], .msg_iovlen = 1 } },
	};
	struct timespec invalid = { .tv_nsec = 1000000000 };
	errno = 0;
	assert(recvmmsg(pair[1], messages, 1, MSG_DONTWAIT, &invalid) == -1);
	assert(errno == EINVAL);
	assert(recvmmsg(pair[1], NULL, 0, MSG_DONTWAIT, NULL) == 0);
	assert(send(pair[0], "a", 1, 0) == 1);
	assert(send(pair[0], "b", 1, 0) == 1);
	struct timespec expired = { 0 };
	assert(recvmmsg(pair[1], messages, 2, 0, &expired) == 1);
	assert(bytes[0] == 'a' && messages[0].msg_len == 1);
	assert(expired.tv_sec == 0 && expired.tv_nsec == 0);
	assert(recvmmsg(pair[1], messages, 2, MSG_WAITFORONE, NULL) == 1);
	assert(bytes[0] == 'b');
	int pending = -1;
	socklen_t length = sizeof(pending);
	assert(getsockopt(pair[1], SOL_SOCKET, SO_ERROR, &pending, &length) ==
	       0);
	assert(pending == 0);
	assert(send(pair[0], "c", 1, 0) == 1);
	messages[1].msg_hdr.msg_iov = NULL;
	assert(recvmmsg(pair[1], messages, 2, 0, NULL) == 1);
	struct pollfd ready = { .fd = pair[1], .events = POLLIN };
	assert(poll(&ready, 1, 0) == 1 && (ready.revents & POLLERR));
	int duplicate = dup(pair[1]);
	assert(duplicate >= 0);
	assert(getsockopt(duplicate, SOL_SOCKET, SO_ERROR, &pending, &length) ==
	       0);
	assert(pending == EFAULT);
	assert(poll(&ready, 1, 0) == 0);
	assert(send(pair[0], "d", 1, 0) == 1);
	assert(recvmmsg(pair[1], messages, 2, 0, NULL) == 1);
	errno = 0;
	assert(recv(duplicate, bytes, 1, MSG_DONTWAIT) == -1 &&
	       errno == EFAULT);
	assert(poll(&ready, 1, 0) == 0);
	long page = sysconf(_SC_PAGESIZE);
	assert(page > 0);
	struct mmsghdr *protected = mmap(NULL, page, PROT_READ | PROT_WRITE,
					 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	assert(protected != MAP_FAILED);
	*protected = messages[0];
	assert(mprotect(protected, page, PROT_READ) == 0);
	assert(send(pair[0], "e", 1, 0) == 1);
	errno = 0;
	assert(syscall(SYS_recvmmsg, pair[1], protected, 1, 0, NULL) == -1);
	assert(errno == EFAULT);
	errno = 0;
	assert(recv(pair[1], bytes, 1, MSG_DONTWAIT) == -1 && errno == EAGAIN);
	assert(munmap(protected, page) == 0);
	assert(close(duplicate) == 0);
	assert(close(pair[0]) == 0);
	assert(close(pair[1]) == 0);
}

int main(void)
{
	errno = 0;
	assert(recvmmsg(-1, NULL, 0, 0, NULL) == -1 && errno == EBADF);
	check_batch(SOCK_DGRAM);
	check_batch(SOCK_SEQPACKET);
	check_batch(SOCK_STREAM);
	puts("PASS recvmmsg prefix, timeout, deferred errors and consumed faults");
	return 0;
}
