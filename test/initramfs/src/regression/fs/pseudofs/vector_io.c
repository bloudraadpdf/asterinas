// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <unistd.h>

static void partial_pipe(void)
{
	int fd[2];
	assert(pipe(fd) == 0);
	assert(write(fd[1], "a", 1) == 1);
	char first = 0, second = 0;
	struct iovec output[] = { { &first, 1 }, { &second, 1 } };
	assert(readv(fd[0], output, 2) == 1);
	assert(first == 'a' && second == 0);
	assert(close(fd[0]) == 0 && close(fd[1]) == 0);
}

static void packet_pipe(void)
{
	int fd[2];
	assert(pipe2(fd, O_DIRECT) == 0);
	struct iovec input[] = { { "ab", 2 }, { "cd", 2 } };
	assert(write(fd[1], "abcd", 4) == 4);
	char first[2] = { 0 }, second[2] = { 0 };
	struct iovec output[] = { { first, 2 }, { second, 2 } };
	assert(readv(fd[0], output, 2) == 4);
	assert(memcmp(first, "ab", 2) == 0 && memcmp(second, "cd", 2) == 0);
	assert(writev(fd[1], input, 2) == 4);
	char packet[4];
	assert(read(fd[0], packet, sizeof(packet)) == 4 &&
	       memcmp(packet, "abcd", 4) == 0);
	assert(close(fd[0]) == 0 && close(fd[1]) == 0);
}

static void datagram_vectors(void)
{
	int fd[2];
	assert(socketpair(AF_UNIX, SOCK_DGRAM, 0, fd) == 0);
	struct iovec input[] = { { "ab", 2 }, { "cd", 2 } };
	assert(send(fd[0], "abcd", 4, 0) == 4);
	char first[2] = { 0 }, second[2] = { 0 };
	struct iovec output[] = { { first, 2 }, { second, 2 } };
	assert(readv(fd[1], output, 2) == 4);
	assert(memcmp(first, "ab", 2) == 0 && memcmp(second, "cd", 2) == 0);
	assert(writev(fd[0], input, 2) == 4);
	char packet[4];
	assert(recv(fd[1], packet, sizeof(packet), 0) == 4 &&
	       memcmp(packet, "abcd", 4) == 0);
	char rest;
	errno = 0;
	assert(recv(fd[1], &rest, 1, MSG_DONTWAIT) == -1 && errno == EAGAIN);
	assert(close(fd[0]) == 0 && close(fd[1]) == 0);
}

static void partial_fault(void)
{
	int fd[2];
	assert(pipe(fd) == 0);
	assert(write(fd[1], "abc", 3) == 3);
	char first[2], remaining[3];
	struct iovec output[] = { { first, 2 }, { (void *)1, 1 } };
	errno = 0;
	assert(readv(fd[0], output, 2) == -1 && errno == EFAULT);
	assert(memcmp(first, "ab", 2) == 0);
	assert(read(fd[0], remaining, sizeof(remaining)) == 3 &&
	       memcmp(remaining, "abc", 3) == 0);

	long page = sysconf(_SC_PAGESIZE);
	assert(page > 0);
	char *data = malloc((size_t)page + 3), *prefix = malloc((size_t)page);
	assert(data != NULL && prefix != NULL);
	memset(data, 'p', (size_t)page);
	memcpy(data + page, "abc", 3);
	assert(write(fd[1], data, (size_t)page + 3) == page + 3);
	struct iovec later[] = { { prefix, (size_t)page },
				 { first, 2 },
				 { (void *)1, 1 } };
	assert(readv(fd[0], later, 3) == page);
	assert(memcmp(prefix, data, (size_t)page) == 0 &&
	       memcmp(first, "ab", 2) == 0);
	assert(read(fd[0], remaining, sizeof(remaining)) == 3 &&
	       memcmp(remaining, "abc", 3) == 0);
	free(prefix);
	free(data);
	assert(close(fd[0]) == 0 && close(fd[1]) == 0);
}

static void file_offset(void)
{
	const char *name = "/tmp/vector-io-regression";
	int fd = open(name, O_CREAT | O_TRUNC | O_RDWR, 0600);
	assert(fd >= 0 && unlink(name) == 0);
	struct iovec input[] = { { "ab", 2 }, { "cd", 2 } };
	assert(writev(fd, input, 2) == 4 && lseek(fd, 0, SEEK_CUR) == 4);
	assert(lseek(fd, 0, SEEK_SET) == 0);
	char first[2] = { 0 }, second[2] = { 0 };
	struct iovec output[] = { { first, 2 }, { second, 2 } };
	assert(preadv(fd, output, 2, 1) == 3 && lseek(fd, 0, SEEK_CUR) == 0);
	assert(readv(fd, output, 2) == 4 && lseek(fd, 0, SEEK_CUR) == 4);
	assert(memcmp(first, "ab", 2) == 0 && memcmp(second, "cd", 2) == 0);
	assert(close(fd) == 0);
}

int main(void)
{
	alarm(10);
	partial_pipe();
	packet_pipe();
	datagram_vectors();
	partial_fault();
	file_offset();
	alarm(0);
	return 0;
}
