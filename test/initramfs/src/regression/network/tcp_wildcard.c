// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <arpa/inet.h>
#include <assert.h>
#include <errno.h>
#include <net/if.h>
#include <poll.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <unistd.h>

static struct sockaddr_in address(in_addr_t ip, in_port_t port)
{
	return (struct sockaddr_in){ .sin_family = AF_INET,
				     .sin_addr.s_addr = ip,
				     .sin_port = port };
}

static struct sockaddr_in local_address(int fd)
{
	struct sockaddr_in result;
	socklen_t size = sizeof(result);
	assert(getsockname(fd, (void *)&result, &size) == 0);
	assert(size == sizeof(result));
	return result;
}

static int bind_address(in_addr_t ip, in_port_t port)
{
	int fd = socket(AF_INET, SOCK_STREAM, 0);
	assert(fd >= 0);
	struct sockaddr_in addr = address(ip, port);
	int result = bind(fd, (void *)&addr, sizeof(addr));
	if (result != 0)
		fprintf(stderr, "bind_address %s:%u: %s\n",
			inet_ntoa(addr.sin_addr), ntohs(port), strerror(errno));
	assert(result == 0);
	return fd;
}

static void roundtrip(int listener, in_addr_t ip, in_port_t port)
{
	int client = socket(AF_INET, SOCK_STREAM, 0);
	assert(client >= 0);
	struct sockaddr_in destination = address(ip, port);
	assert(connect(client, (void *)&destination, sizeof(destination)) == 0);
	struct pollfd event = { .fd = listener, .events = POLLIN };
	assert(poll(&event, 1, 3000) == 1);
	int server = accept(listener, NULL, NULL);
	assert(server >= 0);
	assert(local_address(server).sin_addr.s_addr == ip);
	assert(send(client, "hello", 5, 0) == 5);
	char buffer[5];
	assert(recv(server, buffer, sizeof(buffer), MSG_WAITALL) == 5);
	assert(memcmp(buffer, "hello", 5) == 0);
	assert(send(server, buffer, sizeof(buffer), 0) == 5);
	assert(recv(client, buffer, sizeof(buffer), MSG_WAITALL) == 5);
	assert(memcmp(buffer, "hello", 5) == 0);
	assert(close(server) == 0);
	assert(close(client) == 0);
}

static void bind_conflicts_and_rollback(in_addr_t ethernet)
{
	int blocker = bind_address(ethernet, 0);
	struct sockaddr_in occupied = local_address(blocker);
	int wildcard = socket(AF_INET, SOCK_STREAM, 0);
	assert(wildcard >= 0);
	struct sockaddr_in any = address(htonl(INADDR_ANY), occupied.sin_port);
	errno = 0;
	assert(bind(wildcard, (void *)&any, sizeof(any)) == -1);
	assert(errno == EADDRINUSE);
	int loopback = bind_address(htonl(INADDR_LOOPBACK), occupied.sin_port);
	assert(close(loopback) == 0);
	assert(close(blocker) == 0);
	assert(bind(wildcard, (void *)&any, sizeof(any)) == 0);
	int candidate = socket(AF_INET, SOCK_STREAM, 0);
	assert(candidate >= 0);
	struct sockaddr_in concrete = address(ethernet, occupied.sin_port);
	errno = 0;
	assert(bind(candidate, (void *)&concrete, sizeof(concrete)) == -1);
	assert(errno == EADDRINUSE);
	assert(close(wildcard) == 0);
	assert(bind(candidate, (void *)&concrete, sizeof(concrete)) == 0);
	assert(close(candidate) == 0);
}

static void reusable_listen_rollback(in_addr_t ethernet)
{
	int option = 1;
	int first = socket(AF_INET, SOCK_STREAM, 0);
	int second = socket(AF_INET, SOCK_STREAM, 0);
	assert(first >= 0 && second >= 0);
	assert(setsockopt(first, SOL_SOCKET, SO_REUSEADDR, &option,
			  sizeof(option)) == 0);
	assert(setsockopt(second, SOL_SOCKET, SO_REUSEADDR, &option,
			  sizeof(option)) == 0);
	struct sockaddr_in any = address(htonl(INADDR_ANY), 0);
	assert(bind(first, (void *)&any, sizeof(any)) == 0);
	any = local_address(first);
	assert(bind(second, (void *)&any, sizeof(any)) == 0);
	assert(listen(first, 8) == 0);
	errno = 0;
	assert(listen(second, 8) == -1);
	assert(errno == EADDRINUSE);
	struct sockaddr_in retained = local_address(second);
	assert(retained.sin_addr.s_addr == any.sin_addr.s_addr);
	assert(retained.sin_port == any.sin_port);
	assert(close(first) == 0);
	assert(listen(second, 8) == 0);
	roundtrip(second, htonl(INADDR_LOOPBACK), any.sin_port);
	roundtrip(second, ethernet, any.sin_port);
	assert(close(second) == 0);
}

static void wildcard_connect(in_addr_t destination)
{
	int listener = bind_address(destination, 0);
	assert(listen(listener, 8) == 0);
	struct sockaddr_in peer = local_address(listener);
	int client = bind_address(htonl(INADDR_ANY), 0);
	assert(connect(client, (void *)&peer, sizeof(peer)) == 0);
	assert(local_address(client).sin_addr.s_addr == destination);
	struct pollfd event = { .fd = listener, .events = POLLIN };
	assert(poll(&event, 1, 3000) == 1);
	int accepted = accept(listener, NULL, NULL);
	assert(accepted >= 0);
	assert(close(accepted) == 0);
	assert(close(client) == 0);
	assert(close(listener) == 0);
}

static void implicit_listen(in_addr_t ethernet)
{
	int listener = socket(AF_INET, SOCK_STREAM, 0);
	assert(listener >= 0);
	assert(listen(listener, 8) == 0);
	struct sockaddr_in local = local_address(listener);
	assert(local.sin_addr.s_addr == htonl(INADDR_ANY) &&
	       local.sin_port != 0);
	roundtrip(listener, htonl(INADDR_LOOPBACK), local.sin_port);
	roundtrip(listener, ethernet, local.sin_port);
	assert(close(listener) == 0);
}

static void close_pending(in_addr_t ethernet)
{
	int listener = bind_address(htonl(INADDR_ANY), 0);
	assert(listen(listener, 8) == 0);
	struct sockaddr_in any = local_address(listener);
	int clients[2];
	in_addr_t addresses[] = { htonl(INADDR_LOOPBACK), ethernet };
	for (int i = 0; i < 2; i++) {
		clients[i] = socket(AF_INET, SOCK_STREAM, 0);
		assert(clients[i] >= 0);
		struct sockaddr_in peer = address(addresses[i], any.sin_port);
		assert(connect(clients[i], (void *)&peer, sizeof(peer)) == 0);
	}
	assert(close(listener) == 0);
	for (int i = 0; i < 2; i++) {
		struct pollfd event = { .fd = clients[i], .events = POLLIN };
		assert(poll(&event, 1, 3000) == 1);
		char byte;
		ssize_t result = recv(clients[i], &byte, 1, MSG_DONTWAIT);
		assert(result == 0 || (result == -1 && errno == ECONNRESET));
		assert(close(clients[i]) == 0);
	}
	listener = bind_address(htonl(INADDR_ANY), any.sin_port);
	assert(close(listener) == 0);
}

static void refused_connect_can_select_another_interface(in_addr_t ethernet)
{
	int unused = bind_address(ethernet, 0);
	struct sockaddr_in absent = local_address(unused);
	assert(close(unused) == 0);
	int client = bind_address(htonl(INADDR_ANY), 0);
	errno = 0;
	assert(connect(client, (void *)&absent, sizeof(absent)) == -1);
	assert(errno == ECONNREFUSED);
	int listener = bind_address(htonl(INADDR_ANY), 0);
	assert(listen(listener, 8) == 0);
	struct sockaddr_in peer = address(htonl(INADDR_LOOPBACK),
					  local_address(listener).sin_port);
	assert(connect(client, (void *)&peer, sizeof(peer)) == 0);
	assert(local_address(client).sin_port != 0);
	assert(local_address(client).sin_addr.s_addr == htonl(INADDR_LOOPBACK));
	int accepted = accept(listener, NULL, NULL);
	assert(accepted >= 0);
	assert(close(accepted) == 0);
	assert(close(client) == 0);
	assert(close(listener) == 0);
}

static void ephemeral_port_works_on_every_interface(in_addr_t ethernet)
{
	int seed = bind_address(htonl(INADDR_LOOPBACK), 0);
	unsigned first = ntohs(local_address(seed).sin_port) + 1;
	assert(close(seed) == 0);
	int blocked[4];
	for (unsigned i = 0; i < 4; i++) {
		blocked[i] = socket(AF_INET, SOCK_STREAM, 0);
		assert(blocked[i] >= 0);
		struct sockaddr_in addr = address(ethernet, htons(first + i));
		if (bind(blocked[i], (void *)&addr, sizeof(addr)) != 0) {
			assert(errno == EADDRINUSE);
			assert(close(blocked[i]) == 0);
			blocked[i] = -1;
		}
	}
	int wildcard = bind_address(htonl(INADDR_ANY), 0);
	unsigned selected = ntohs(local_address(wildcard).sin_port);
	assert(selected < first || selected >= first + 4);
	assert(close(wildcard) == 0);
	for (int i = 0; i < 4; i++) {
		if (blocked[i] >= 0)
			assert(close(blocked[i]) == 0);
	}
}

struct competing_bind {
	pthread_barrier_t *barrier;
	struct sockaddr_in address;
	int fd;
	int result;
	int error;
};

static void *bind_competitor(void *argument)
{
	struct competing_bind *attempt = argument;
	attempt->fd = socket(AF_INET, SOCK_STREAM, 0);
	assert(attempt->fd >= 0);
	int status = pthread_barrier_wait(attempt->barrier);
	assert(status == 0 || status == PTHREAD_BARRIER_SERIAL_THREAD);
	attempt->result = bind(attempt->fd, (void *)&attempt->address,
			       sizeof(attempt->address));
	attempt->error = errno;
	return NULL;
}

static void concurrent_bind_is_atomic(in_addr_t ethernet)
{
	for (int iteration = 0; iteration < 32; iteration++) {
		int seed = bind_address(htonl(INADDR_ANY), 0);
		in_port_t port = local_address(seed).sin_port;
		assert(close(seed) == 0);
		pthread_barrier_t barrier;
		assert(pthread_barrier_init(&barrier, NULL, 3) == 0);
		struct competing_bind attempts[2] = {
			{ .barrier = &barrier,
			  .address = address(htonl(INADDR_ANY), port) },
			{ .barrier = &barrier,
			  .address = address(ethernet, port) },
		};
		pthread_t threads[2];
		for (int i = 0; i < 2; i++)
			assert(pthread_create(&threads[i], NULL,
					      bind_competitor,
					      &attempts[i]) == 0);
		int status = pthread_barrier_wait(&barrier);
		assert(status == 0 || status == PTHREAD_BARRIER_SERIAL_THREAD);
		for (int i = 0; i < 2; i++)
			assert(pthread_join(threads[i], NULL) == 0);
		assert((attempts[0].result == 0) + (attempts[1].result == 0) ==
		       1);
		for (int i = 0; i < 2; i++) {
			assert(attempts[i].result == 0 ||
			       attempts[i].error == EADDRINUSE);
			assert(close(attempts[i].fd) == 0);
		}
		assert(pthread_barrier_destroy(&barrier) == 0);
		int released = bind_address(htonl(INADDR_ANY), port);
		assert(close(released) == 0);
	}
}

static void external_roundtrip(in_addr_t ethernet)
{
	if (getenv("TCP_WILDCARD_EXTERNAL") == NULL)
		return;
	int listener = bind_address(htonl(INADDR_ANY), 0);
	assert(listen(listener, 8) == 0);
	printf("TCP_EXTERNAL_PORT=%u\n",
	       ntohs(local_address(listener).sin_port));
	struct pollfd event = { .fd = listener, .events = POLLIN };
	assert(poll(&event, 1, 5000) == 1);
	int accepted = accept(listener, NULL, NULL);
	assert(accepted >= 0);
	assert(local_address(accepted).sin_addr.s_addr == ethernet);
	char data[8];
	assert(recv(accepted, data, sizeof(data), MSG_WAITALL) == sizeof(data));
	assert(memcmp(data, "external", sizeof(data)) == 0);
	assert(send(accepted, data, sizeof(data), 0) == sizeof(data));
	assert(close(accepted) == 0);
	assert(close(listener) == 0);
	puts("TCP_EXTERNAL_OK");
}

static void ipv6_loopback(void)
{
	int listener = socket(AF_INET6, SOCK_STREAM, 0);
	int client = socket(AF_INET6, SOCK_STREAM, 0);
	assert(listener >= 0 && client >= 0);
	struct sockaddr_in6 any = { .sin6_family = AF_INET6 };
	assert(bind(listener, (void *)&any, sizeof(any)) == 0);
	assert(listen(listener, 8) == 0);
	socklen_t size = sizeof(any);
	assert(getsockname(listener, (void *)&any, &size) == 0);
	assert(IN6_IS_ADDR_UNSPECIFIED(&any.sin6_addr) && any.sin6_port != 0);
	any.sin6_addr = in6addr_loopback;
	assert(connect(client, (void *)&any, sizeof(any)) == 0);
	struct pollfd event = { .fd = listener, .events = POLLIN };
	assert(poll(&event, 1, 3000) == 1);
	int accepted = accept(listener, NULL, NULL);
	assert(accepted >= 0);
	assert(close(accepted) == 0);
	assert(close(client) == 0);
	assert(close(listener) == 0);
}

int main(void)
{
	setbuf(stdout, NULL);
	ipv6_loopback();
	int listener = bind_address(htonl(INADDR_ANY), 0);
	struct sockaddr_in local = local_address(listener);
	assert(local.sin_addr.s_addr == htonl(INADDR_ANY));
	assert(local.sin_port != 0);
	assert(listen(listener, 8) == 0);
	roundtrip(listener, htonl(INADDR_LOOPBACK), local.sin_port);
	struct ifreq request = { 0 };
	strcpy(request.ifr_name, "eth0");
	if (ioctl(listener, SIOCGIFADDR, &request) == 0) {
		struct sockaddr_in *ethernet = (void *)&request.ifr_addr;
		in_addr_t ip = ethernet->sin_addr.s_addr;
		if (ip == htonl(INADDR_ANY)) {
			assert(close(listener) == 0);
			puts("TCP_WILDCARD_OK");
			return 0;
		}
		roundtrip(listener, ip, local.sin_port);
		bind_conflicts_and_rollback(ip);
		reusable_listen_rollback(ip);
		wildcard_connect(htonl(INADDR_LOOPBACK));
		wildcard_connect(ip);
		implicit_listen(ip);
		close_pending(ip);
		refused_connect_can_select_another_interface(ip);
		ephemeral_port_works_on_every_interface(ip);
		concurrent_bind_is_atomic(ip);
		external_roundtrip(ip);
		puts("TCP_WILDCARD_ETHERNET_OK");
	}
	assert(close(listener) == 0);
	puts("TCP_WILDCARD_OK");
	return 0;
}
