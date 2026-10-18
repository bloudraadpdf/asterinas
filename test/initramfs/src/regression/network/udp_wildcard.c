// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <arpa/inet.h>
#include <assert.h>
#include <errno.h>
#include <net/if.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <unistd.h>

static struct sockaddr_in address(in_addr_t ip, in_port_t port)
{
	struct sockaddr_in result = {
		.sin_family = AF_INET,
		.sin_addr.s_addr = ip,
		.sin_port = port,
	};
	return result;
}

static struct sockaddr_in local(int fd)
{
	struct sockaddr_in result;
	socklen_t size = sizeof(result);
	assert(getsockname(fd, (struct sockaddr *)&result, &size) == 0);
	assert(size == sizeof(result));
	return result;
}

static int bind_with_reuse(in_addr_t ip, in_port_t port, int reuse)
{
	int fd = socket(AF_INET, SOCK_DGRAM, 0);
	assert(fd >= 0);
	struct timeval timeout = { .tv_sec = 3 };
	assert(setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout,
			  sizeof(timeout)) == 0);
	if (reuse) {
		assert(setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &reuse,
				  sizeof(reuse)) == 0);
	}
	struct sockaddr_in endpoint = address(ip, port);
	assert(bind(fd, (struct sockaddr *)&endpoint, sizeof(endpoint)) == 0);
	return fd;
}

static int bind_socket(in_addr_t ip, in_port_t port)
{
	return bind_with_reuse(ip, port, 0);
}

static void send_byte(int fd, const struct sockaddr_in *to, char value)
{
	assert(sendto(fd, &value, 1, 0, (const struct sockaddr *)to,
		      sizeof(*to)) == 1);
}

static void receive_byte(int fd, char value, int flags)
{
	char received = 0;
	assert(recv(fd, &received, 1, flags) == 1);
	assert(received == value);
}

static void connected_filter(void)
{
	int receiver = bind_socket(htonl(INADDR_LOOPBACK), 0);
	int peer = bind_socket(htonl(INADDR_LOOPBACK), 0);
	int other = bind_socket(htonl(INADDR_LOOPBACK), 0);
	struct sockaddr_in destination = local(receiver);
	struct sockaddr_in source = local(peer);
	int other_address =
		bind_socket(inet_addr("127.0.0.2"), source.sin_port);
	send_byte(other, &destination, 'q');
	assert(connect(receiver, (struct sockaddr *)&source, sizeof(source)) ==
	       0);
	receive_byte(receiver, 'q', 0);
	send_byte(other, &destination, 'x');
	send_byte(other_address, &destination, 'a');
	send_byte(peer, &destination, 'p');
	receive_byte(receiver, 'p', MSG_PEEK);
	receive_byte(receiver, 'p', 0);
	assert(recv(receiver, &destination, sizeof(destination),
		    MSG_DONTWAIT) == -1);
	assert(errno == EAGAIN || errno == EWOULDBLOCK);
	close(other);
	close(other_address);
	close(peer);
	close(receiver);
	puts("UDP_CONNECTED_FILTER_OK");
}

static void reuse_selection(void)
{
	int first = bind_with_reuse(htonl(INADDR_ANY), 0, 1);
	in_port_t port = local(first).sin_port;
	int second = bind_with_reuse(htonl(INADDR_ANY), port, 1);
	int third = bind_with_reuse(htonl(INADDR_ANY), port, 1);
	int peer = bind_socket(htonl(INADDR_LOOPBACK), 0);
	struct sockaddr_in target = address(htonl(INADDR_LOOPBACK), port);
	close(first);
	send_byte(peer, &target, 'l');
	receive_byte(third, 'l', 0);
	char byte;
	assert(recv(second, &byte, 1, MSG_DONTWAIT) == -1 && errno == EAGAIN);
	int specific = bind_with_reuse(htonl(INADDR_LOOPBACK), port, 1);
	send_byte(peer, &target, 's');
	receive_byte(specific, 's', 0);
	assert(recv(third, &byte, 1, MSG_DONTWAIT) == -1 && errno == EAGAIN);
	struct sockaddr_in remote = local(peer);
	assert(connect(second, (struct sockaddr *)&remote, sizeof(remote)) ==
	       0);
	send_byte(peer, &target, 'c');
	receive_byte(second, 'c', 0);
	assert(recv(specific, &byte, 1, MSG_DONTWAIT) == -1 && errno == EAGAIN);
	int other = bind_socket(htonl(INADDR_LOOPBACK), 0);
	send_byte(other, &target, 'o');
	receive_byte(specific, 'o', 0);
	assert(recv(second, &byte, 1, MSG_DONTWAIT) == -1 && errno == EAGAIN);
	close(other);
	close(specific);
	close(peer);
	close(third);
	close(second);
	puts("UDP_REUSE_SELECTION_OK");
}

static void wildcard(in_addr_t ethernet)
{
	int receiver = bind_socket(htonl(INADDR_ANY), 0);
	int loopback = bind_socket(htonl(INADDR_LOOPBACK), 0);
	int external = bind_socket(ethernet, 0);
	struct sockaddr_in bound = local(receiver);
	assert(bound.sin_addr.s_addr == htonl(INADDR_ANY));
	assert(bound.sin_port != 0);
	struct sockaddr_in destination =
		address(htonl(INADDR_LOOPBACK), bound.sin_port);
	send_byte(loopback, &destination, 'l');
	destination.sin_addr.s_addr = ethernet;
	send_byte(external, &destination, 'e');
	receive_byte(receiver, 'l', MSG_PEEK);
	receive_byte(receiver, 'l', 0);
	receive_byte(receiver, 'e', 0);

	struct sockaddr_in targets[] = { local(loopback), local(external) };
	for (size_t i = 0; i < sizeof(targets) / sizeof(targets[0]); ++i) {
		send_byte(receiver, &targets[i], 'r');
		struct sockaddr_in source;
		socklen_t size = sizeof(source);
		char value;
		assert(recvfrom(i == 0 ? loopback : external, &value, 1, 0,
				(struct sockaddr *)&source, &size) == 1);
		assert(value == 'r');
		assert(source.sin_addr.s_addr == targets[i].sin_addr.s_addr);
		assert(source.sin_port == bound.sin_port);
	}

	int conflict = socket(AF_INET, SOCK_DGRAM, 0);
	assert(conflict >= 0);
	assert(bind(conflict, (struct sockaddr *)&destination,
		    sizeof(destination)) == -1);
	assert(errno == EADDRINUSE);
	close(receiver);
	assert(bind(conflict, (struct sockaddr *)&destination,
		    sizeof(destination)) == 0);
	close(conflict);
	close(loopback);
	close(external);
	puts("UDP_WILDCARD_ETHERNET_OK");
}

static void implicit_binding(void)
{
	int receiver = bind_socket(htonl(INADDR_LOOPBACK), 0);
	int sender = socket(AF_INET, SOCK_DGRAM, 0);
	assert(sender >= 0);
	struct sockaddr_in destination = local(receiver);
	send_byte(sender, &destination, 'i');
	receive_byte(receiver, 'i', 0);
	struct sockaddr_in bound = local(sender);
	assert(bound.sin_addr.s_addr == htonl(INADDR_ANY));
	assert(bound.sin_port != 0);
	assert(connect(sender, (struct sockaddr *)&destination,
		       sizeof(destination)) == 0);
	struct sockaddr_in connected = local(sender);
	assert(connected.sin_addr.s_addr == htonl(INADDR_LOOPBACK));
	assert(connected.sin_port == bound.sin_port);
	close(sender);
	close(receiver);
	puts("UDP_IMPLICIT_BIND_OK");
}

static void failed_copy(void)
{
	int receiver = bind_socket(htonl(INADDR_LOOPBACK), 0);
	int sender = bind_socket(htonl(INADDR_LOOPBACK), 0);
	struct sockaddr_in destination = local(receiver);
	long page = sysconf(_SC_PAGESIZE);
	assert(page > 0);
	char *buffer = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE,
			    MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	assert(buffer != MAP_FAILED);
	memset(buffer, 'f', page);
	assert(mprotect(buffer + page, page, PROT_NONE) == 0);
	assert(sendto(sender, buffer + page - 1, 2, 0,
		      (struct sockaddr *)&destination,
		      sizeof(destination)) == -1);
	assert(errno == EFAULT);
	send_byte(sender, &destination, 'v');
	receive_byte(receiver, 'v', 0);
	assert(recv(receiver, buffer, 1, MSG_DONTWAIT) == -1);
	assert(errno == EAGAIN || errno == EWOULDBLOCK);
	assert(munmap(buffer, 2 * page) == 0);
	close(sender);
	close(receiver);
	puts("UDP_FAILED_COPY_OK");
}

static void external_echo(void)
{
	int fd = bind_socket(htonl(INADDR_ANY), 0);
	printf("UDP_EXTERNAL_PORT=%u\n", ntohs(local(fd).sin_port));
	struct sockaddr_in source;
	socklen_t size = sizeof(source);
	char buffer[8];
	assert(recvfrom(fd, buffer, sizeof(buffer), 0,
			(struct sockaddr *)&source, &size) == 8);
	assert(memcmp(buffer, "external", 8) == 0);
	assert(sendto(fd, buffer, sizeof(buffer), 0, (struct sockaddr *)&source,
		      size) == 8);
	close(fd);
	puts("UDP_EXTERNAL_OK");
}

int main(int argc, char **argv)
{
	setvbuf(stdout, NULL, _IONBF, 0);
	if (argc == 2 && strcmp(argv[1], "copy") == 0) {
		failed_copy();
		return 0;
	}
	if (argc == 2 && strcmp(argv[1], "implicit") == 0) {
		implicit_binding();
		return 0;
	}
	connected_filter();
	reuse_selection();
	implicit_binding();
	failed_copy();
	int query = socket(AF_INET, SOCK_DGRAM, 0);
	assert(query >= 0);
	struct ifreq request = { 0 };
	strcpy(request.ifr_name, "eth0");
	if (ioctl(query, SIOCGIFADDR, &request) == 0) {
		in_addr_t ip = ((struct sockaddr_in *)&request.ifr_addr)
				       ->sin_addr.s_addr;
		if (ip != htonl(INADDR_ANY)) {
			wildcard(ip);
		}
	}
	close(query);
	if (getenv("UDP_WILDCARD_EXTERNAL")) {
		external_echo();
	}
	puts("UDP_WILDCARD_OK");
	return 0;
}
