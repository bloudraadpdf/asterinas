// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <stdio.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <unistd.h>

static void check_value(int fd, int expected)
{
	int value = -1;
	socklen_t length = sizeof(value);
	assert(getsockopt(fd, IPPROTO_TCP, TCP_FASTOPEN, &value, &length) == 0);
	assert(length == sizeof(value) && value == expected);
}

static void check_memory(int fd)
{
	long page = sysconf(_SC_PAGESIZE);
	assert(page > 0);
	char *area = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE,
			  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	assert(area != MAP_FAILED);
	assert(mprotect(area + page, page, PROT_NONE) == 0);
	assert(setsockopt(fd, IPPROTO_TCP, TCP_FASTOPEN, area + page, 4) ==
		       -1 &&
	       errno == EFAULT);
	assert(setsockopt(fd, IPPROTO_TCP, TCP_FASTOPEN, area + page - 2, 4) ==
		       -1 &&
	       errno == EFAULT);
	assert(setsockopt(fd, IPPROTO_TCP, TCP_FASTOPEN, area + page, 1) ==
		       -1 &&
	       errno == EINVAL);
	socklen_t length = 4;
	assert(getsockopt(fd, IPPROTO_TCP, TCP_FASTOPEN, area + page,
			  &length) == -1 &&
	       errno == EFAULT);
	length = 0;
	assert(getsockopt(fd, IPPROTO_TCP, TCP_FASTOPEN, area + page,
			  &length) == 0 &&
	       length == 0);
	assert(munmap(area, 2 * page) == 0);
}

int main(void)
{
	int server = socket(AF_INET, SOCK_STREAM, 0);
	assert(server >= 0);
	check_value(server, 0);
	check_memory(server);
	int value = -1;
	assert(setsockopt(server, IPPROTO_TCP, TCP_FASTOPEN, &value,
			  sizeof(value)) == -1 &&
	       errno == EINVAL);
	check_value(server, 0);
	value = 16;
	assert(setsockopt(server, IPPROTO_TCP, TCP_FASTOPEN, &value,
			  sizeof(value)) == 0);
	check_value(server, 16);
	struct sockaddr_in address = {
		.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)
	};
	assert(bind(server, (void *)&address, sizeof(address)) == 0);
	assert(listen(server, 4) == 0);
	value = 0;
	assert(setsockopt(server, IPPROTO_TCP, TCP_FASTOPEN, &value,
			  sizeof(value)) == 0);
	check_value(server, 0);
	value = 16;
	assert(setsockopt(server, IPPROTO_TCP, TCP_FASTOPEN, &value,
			  sizeof(value)) == 0);
	check_value(server, 16);
	socklen_t length = sizeof(address);
	assert(getsockname(server, (void *)&address, &length) == 0);
	int client = socket(AF_INET, SOCK_STREAM, 0);
	assert(client >= 0 &&
	       connect(client, (void *)&address, sizeof(address)) == 0);
	int accepted = accept(server, NULL, NULL);
	assert(accepted >= 0);
	assert(setsockopt(client, IPPROTO_TCP, TCP_FASTOPEN, &value,
			  sizeof(value)) == -1 &&
	       errno == EINVAL);
	assert(setsockopt(accepted, IPPROTO_TCP, TCP_FASTOPEN, &value,
			  sizeof(value)) == -1 &&
	       errno == EINVAL);
	assert(send(client, "a", 1, MSG_NOSIGNAL) == 1);
	char byte;
	assert(recv(accepted, &byte, 1, 0) == 1 && byte == 'a');
	assert(close(client) == 0 && close(accepted) == 0 &&
	       close(server) == 0);
	puts("PASS Fast Open option memory, state, live listener and normal fallback");
	return 0;
}
