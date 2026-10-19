/* SPDX-License-Identifier: MPL-2.0 */
#define _GNU_SOURCE
#include <assert.h>
#include <fcntl.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <unistd.h>

static void send_rights(int socket)
{
	char byte = 'x';
	struct iovec vector = { &byte, 1 };
	union {
		struct cmsghdr alignment;
		char bytes[CMSG_SPACE(2 * sizeof(int))];
	} control = { 0 };
	struct msghdr message = { .msg_iov = &vector,
				  .msg_iovlen = 1,
				  .msg_control = control.bytes,
				  .msg_controllen = sizeof(control.bytes) };
	struct cmsghdr *header = CMSG_FIRSTHDR(&message);
	header->cmsg_level = SOL_SOCKET;
	header->cmsg_type = SCM_RIGHTS;
	header->cmsg_len = CMSG_LEN(2 * sizeof(int));
	int files[] = { STDOUT_FILENO, STDOUT_FILENO };
	memcpy(CMSG_DATA(header), files, sizeof(files));
	assert(sendmsg(socket, &message, 0) == 1);
}

static int receive_rights(int socket, int expected)
{
	char byte;
	struct iovec vector = { &byte, 1 };
	union {
		struct cmsghdr alignment;
		char bytes[CMSG_SPACE(2 * sizeof(int))];
	} control = { 0 };
	struct msghdr message = { .msg_iov = &vector,
				  .msg_iovlen = 1,
				  .msg_control = control.bytes,
				  .msg_controllen = sizeof(control.bytes) };
	assert(recvmsg(socket, &message, MSG_CMSG_CLOEXEC) == 1 && byte == 'x');
	assert(message.msg_flags & MSG_CTRUNC);
	struct cmsghdr *header = CMSG_FIRSTHDR(&message);
	if (!expected) {
		assert(header == NULL);
		return -1;
	}
	assert(header != NULL && header->cmsg_len == CMSG_LEN(sizeof(int)));
	int fd;
	memcpy(&fd, CMSG_DATA(header), sizeof(fd));
	assert(fcntl(fd, F_GETFD) == FD_CLOEXEC);
	return fd;
}

int main(void)
{
	int pair[2], opened[16], count = 0, fd;
	struct rlimit original, limit;
	assert(getrlimit(RLIMIT_NOFILE, &original) == 0);
	assert(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0);
	send_rights(pair[0]);
	send_rights(pair[0]);
	limit = original;
	limit.rlim_cur = 10;
	assert(setrlimit(RLIMIT_NOFILE, &limit) == 0);
	while ((fd = dup(STDOUT_FILENO)) >= 0) {
		assert(count < 16);
		opened[count++] = fd;
	}
	assert(count > 0 && opened[count - 1] == 9);
	assert(close(opened[--count]) == 0);
	fd = receive_rights(pair[1], 1);
	assert(fd == 9 && close(fd) == 0);
	limit.rlim_cur = 0;
	assert(setrlimit(RLIMIT_NOFILE, &limit) == 0);
	assert(receive_rights(pair[1], 0) == -1);
	assert(setrlimit(RLIMIT_NOFILE, &original) == 0);
	while (count)
		assert(close(opened[--count]) == 0);
	assert(close(pair[0]) == 0 && close(pair[1]) == 0);
	return 0;
}
