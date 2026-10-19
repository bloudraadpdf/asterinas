// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <assert.h>
#include <netinet/in.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

static void check_timestamp(int sender, int receiver, int option,
			    const struct sockaddr_in *destination)
{
	int enabled = 1;
	assert(setsockopt(receiver, SOL_SOCKET, option, &enabled,
			  sizeof(enabled)) == 0);
	assert(send(sender, "t", 1, 0) == 1);
	struct pollfd ready = { .fd = receiver, .events = POLLIN };
	assert(poll(&ready, 1, 1000) == 1 && (ready.revents & POLLIN));
	struct timespec queued;
	assert(clock_gettime(CLOCK_REALTIME, &queued) == 0);
	assert(nanosleep(&(struct timespec){ .tv_nsec = 20000000 }, NULL) == 0);
	int64_t arrival[2] = { 0 };
	for (int pass = 0; pass < 2; ++pass) {
		union {
			struct cmsghdr align;
			char bytes[128];
		} control = { 0 };
		char byte = 0;
		struct iovec vector = { &byte, 1 };
		struct msghdr message = { .msg_iov = &vector,
					  .msg_iovlen = 1,
					  .msg_control = control.bytes,
					  .msg_controllen =
						  sizeof(control.bytes) };
		assert(recvmsg(receiver, &message, pass == 0 ? MSG_PEEK : 0) ==
		       1);
		assert(byte == 't' && !(message.msg_flags & MSG_CTRUNC));
		unsigned stamps = 0, destinations = 0;
		for (struct cmsghdr *header = CMSG_FIRSTHDR(&message); header;
		     header = CMSG_NXTHDR(&message, header)) {
			if (header->cmsg_level == SOL_SOCKET &&
			    header->cmsg_type == option) {
				int64_t stamp[2];
				assert(header->cmsg_len ==
				       CMSG_LEN(sizeof(stamp)));
				memcpy(stamp, CMSG_DATA(header), sizeof(stamp));
				if (option == SO_TIMESTAMP_OLD ||
				    option == SO_TIMESTAMP_NEW)
					stamp[1] *= 1000;
				assert(stamp[0] > 0 && stamp[1] >= 0 &&
				       stamp[1] < 1000000000);
				assert(stamp[0] < queued.tv_sec ||
				       (stamp[0] == queued.tv_sec &&
					stamp[1] <= queued.tv_nsec));
				if (pass == 0)
					memcpy(arrival, stamp, sizeof(stamp));
				else
					assert(memcmp(arrival, stamp,
						      sizeof(stamp)) == 0);
				++stamps;
			} else if (header->cmsg_level == SOL_IP &&
				   header->cmsg_type == IP_ORIGDSTADDR) {
				struct sockaddr_in actual;
				assert(header->cmsg_len ==
				       CMSG_LEN(sizeof(actual)));
				memcpy(&actual, CMSG_DATA(header),
				       sizeof(actual));
				assert(actual.sin_family == AF_INET);
				assert(actual.sin_addr.s_addr ==
				       destination->sin_addr.s_addr);
				assert(actual.sin_port ==
				       destination->sin_port);
				++destinations;
			}
		}
		assert(stamps == 1 && destinations == 1);
	}
}

int main(void)
{
	int receiver = socket(AF_INET, SOCK_DGRAM, 0);
	int sender = socket(AF_INET, SOCK_DGRAM, 0);
	assert(receiver >= 0 && sender >= 0);
	struct sockaddr_in destination = {
		.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)
	};
	assert(bind(receiver, (struct sockaddr *)&destination,
		    sizeof(destination)) == 0);
	socklen_t length = sizeof(destination);
	assert(getsockname(receiver, (struct sockaddr *)&destination,
			   &length) == 0);
	assert(connect(sender, (struct sockaddr *)&destination, length) == 0);
	int enabled = 1;
	assert(setsockopt(receiver, SOL_IP, IP_RECVORIGDSTADDR, &enabled,
			  sizeof(enabled)) == 0);
	int options[] = { SO_TIMESTAMP_OLD, SO_TIMESTAMPNS_OLD,
			  SO_TIMESTAMP_NEW, SO_TIMESTAMPNS_NEW };
	for (unsigned index = 0; index < sizeof(options) / sizeof(options[0]);
	     ++index)
		check_timestamp(sender, receiver, options[index], &destination);
	assert(close(sender) == 0);
	assert(close(receiver) == 0);
	puts("PASS arrival timestamps, old/new layouts, peek and original destination");
	return 0;
}
