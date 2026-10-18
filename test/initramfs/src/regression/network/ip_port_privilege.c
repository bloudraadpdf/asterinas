// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <arpa/inet.h>
#include <assert.h>
#include <errno.h>
#include <grp.h>
#include <linux/capability.h>
#include <stdio.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <unistd.h>

static int new_socket(int family, int type)
{
	int fd = socket(family, type, 0);
	assert(fd >= 0);
	struct timeval timeout = { .tv_sec = 3 };
	assert(setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout,
			  sizeof(timeout)) == 0);
	assert(setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &timeout,
			  sizeof(timeout)) == 0);
	return fd;
}

static void bind_ports(int family, int type, int wildcard)
{
	struct sockaddr_storage endpoint = { 0 };
	socklen_t size;
	in_port_t *port;
	if (family == AF_INET) {
		struct sockaddr_in *ipv4 = (struct sockaddr_in *)&endpoint;
		ipv4->sin_family = AF_INET;
		ipv4->sin_addr.s_addr =
			htonl(wildcard ? INADDR_ANY : INADDR_LOOPBACK);
		port = &ipv4->sin_port;
		size = sizeof(*ipv4);
	} else {
		struct sockaddr_in6 *ipv6 = (struct sockaddr_in6 *)&endpoint;
		ipv6->sin6_family = AF_INET6;
		ipv6->sin6_addr = wildcard ? in6addr_any : in6addr_loopback;
		port = &ipv6->sin6_port;
		size = sizeof(*ipv6);
	}
	for (int number = 0; number < 2; ++number) {
		int fd = new_socket(family, type);
		*port = htons(number == 0 ? 1 : 1023);
		errno = 0;
		assert(bind(fd, (struct sockaddr *)&endpoint, size) == -1);
		assert(errno == EACCES);
		close(fd);
	}
	for (int number = 0; number < 2; ++number) {
		int fd = new_socket(family, type);
		*port = htons(number == 0 ? 0 : 1024);
		assert(bind(fd, (struct sockaddr *)&endpoint, size) == 0);
		socklen_t actual_size = size;
		assert(getsockname(fd, (struct sockaddr *)&endpoint,
				   &actual_size) == 0);
		assert(actual_size == size);
		assert(ntohs(*port) >= 1024);
		if (number != 0) {
			assert(ntohs(*port) == 1024);
		}
		close(fd);
	}
}

static void implicit_ports(int type)
{
	int receiver = new_socket(AF_INET, type);
	struct sockaddr_in endpoint = {
		.sin_family = AF_INET,
		.sin_addr.s_addr = htonl(INADDR_LOOPBACK),
	};
	assert(bind(receiver, (struct sockaddr *)&endpoint, sizeof(endpoint)) ==
	       0);
	socklen_t size = sizeof(endpoint);
	assert(getsockname(receiver, (struct sockaddr *)&endpoint, &size) == 0);
	if (type == SOCK_STREAM) {
		assert(listen(receiver, 1) == 0);
	}
	int sender = new_socket(AF_INET, type);
	assert(connect(sender, (struct sockaddr *)&endpoint,
		       sizeof(endpoint)) == 0);
	struct sockaddr_in source;
	size = sizeof(source);
	assert(getsockname(sender, (struct sockaddr *)&source, &size) == 0);
	assert(source.sin_addr.s_addr == htonl(INADDR_LOOPBACK));
	assert(ntohs(source.sin_port) >= 1024);
	int accepted = type == SOCK_STREAM ? accept(receiver, NULL, NULL) :
					     receiver;
	assert(accepted >= 0);
	assert(send(sender, "x", 1, 0) == 1);
	char value;
	assert(recv(accepted, &value, 1, 0) == 1 && value == 'x');
	if (accepted != receiver) {
		close(accepted);
	}
	close(sender);
	close(receiver);
}

int main(void)
{
	if (geteuid() == 0) {
		assert(setgroups(0, NULL) == 0);
		assert(setgid(10001) == 0);
		assert(setuid(10001) == 0);
	}
	assert(geteuid() != 0);
	struct __user_cap_header_struct header = {
		.version = _LINUX_CAPABILITY_VERSION_3,
	};
	struct __user_cap_data_struct capabilities[2] = { 0 };
	assert(syscall(SYS_capset, &header, capabilities) == 0);
	assert(syscall(SYS_capget, &header, capabilities) == 0);
	for (int index = 0; index < 2; ++index) {
		assert(capabilities[index].effective == 0);
		assert(capabilities[index].permitted == 0);
		assert(capabilities[index].inheritable == 0);
	}
	const struct {
		int family;
		int type;
	} cases[] = {
		{ AF_INET, SOCK_STREAM },
		{ AF_INET, SOCK_DGRAM },
		{ AF_INET6, SOCK_STREAM },
	};
	for (size_t index = 0; index < sizeof(cases) / sizeof(cases[0]);
	     ++index) {
		for (int wildcard = 0; wildcard < 2; ++wildcard) {
			bind_ports(cases[index].family, cases[index].type,
				   wildcard);
		}
	}
	implicit_ports(SOCK_STREAM);
	implicit_ports(SOCK_DGRAM);
	puts("IP_PORT_PRIVILEGE_OK");
	return 0;
}
