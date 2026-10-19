/* SPDX-License-Identifier: MPL-2.0 */

#define _GNU_SOURCE
#include <errno.h>
#include <limits.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

#define VERSION_1 UINT32_C(0x19980330)
#define VERSION_2 UINT32_C(0x20071026)
#define VERSION_3 UINT32_C(0x20080522)
#define REQUIRE(x)                                                          \
	do {                                                                \
		if (!(x)) {                                                 \
			fprintf(stderr, "line=%d expression=%s errno=%d\n", \
				__LINE__, #x, errno);                       \
			return 1;                                           \
		}                                                           \
	} while (0)
#define ERROR(expression, expected)                           \
	do {                                                  \
		errno = 0;                                    \
		long result = (expression);                   \
		REQUIRE(result == -1 && errno == (expected)); \
	} while (0)

struct cap_header {
	uint32_t version;
	int32_t pid;
};

struct cap_data {
	uint32_t effective, permitted, inheritable;
};

_Static_assert(sizeof(struct cap_header) == 8, "header ABI size");
_Static_assert(sizeof(struct cap_data) == 12, "data ABI size");

int main(void)
{
	struct cap_header header = { VERSION_3, 0 };
	struct cap_data original[2], data[2], poisoned[2];
	REQUIRE(syscall(SYS_capget, &header, original) == 0);
	ERROR(syscall(SYS_capget, NULL, NULL), EFAULT);
	ERROR(syscall(SYS_capset, NULL, NULL), EFAULT);

	header = (struct cap_header){ 0, -1 };
	REQUIRE(syscall(SYS_capget, &header, NULL) == 0);
	REQUIRE(header.version == VERSION_3 && header.pid == -1);
	REQUIRE(syscall(SYS_capget, &header, NULL) == 0);
	header.version = 0;
	ERROR(syscall(SYS_capget, &header, (void *)1), EINVAL);
	REQUIRE(header.version == VERSION_3);
	header.version = 0;
	ERROR(syscall(SYS_capset, &header, (void *)1), EINVAL);
	REQUIRE(header.version == VERSION_3);

	ERROR(syscall(SYS_capget, &header, (void *)1), EINVAL);
	ERROR(syscall(SYS_capset, &header, (void *)1), EPERM);
	header.pid = INT_MAX;
	ERROR(syscall(SYS_capget, &header, (void *)1), ESRCH);
	ERROR(syscall(SYS_capset, &header, (void *)1), EPERM);
	header.pid = 0;
	ERROR(syscall(SYS_capget, &header, (void *)1), EFAULT);
	ERROR(syscall(SYS_capset, &header, NULL), EFAULT);
	header.pid = (int32_t)syscall(SYS_gettid);
	REQUIRE(syscall(SYS_capget, &header, data) == 0);
	REQUIRE(memcmp(original, data, sizeof(data)) == 0);
	REQUIRE(syscall(SYS_capset, &header, original) == 0);

	memset(data, 0xa5, sizeof(data));
	memcpy(poisoned, data, sizeof(data));
	header = (struct cap_header){ VERSION_1, 0 };
	REQUIRE(syscall(SYS_capget, &header, data) == 0);
	REQUIRE(memcmp(&original[0], &data[0], sizeof(data[0])) == 0);
	REQUIRE(memcmp(&poisoned[1], &data[1], sizeof(data[1])) == 0);
	header.version = VERSION_2;
	REQUIRE(syscall(SYS_capget, &header, data) == 0);
	REQUIRE(memcmp(original, data, sizeof(data)) == 0);

	long page = sysconf(_SC_PAGESIZE);
	REQUIRE(page > 0);
	unsigned char *mapping = mmap(NULL, (size_t)page * 2,
				      PROT_READ | PROT_WRITE,
				      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	REQUIRE(mapping != MAP_FAILED);
	REQUIRE(mprotect(mapping + page, (size_t)page, PROT_NONE) == 0);
	struct cap_header *split = (void *)(mapping + page - 4);
	split->version = 0;
	REQUIRE(syscall(SYS_capget, split, NULL) == 0);
	REQUIRE(split->version == VERSION_3);
	REQUIRE(syscall(SYS_capget, split, NULL) == 0);
	ERROR(syscall(SYS_capset, split, original), EFAULT);
	split->version = 0;
	struct cap_header *readonly = (void *)mapping;
	*readonly = (struct cap_header){ VERSION_3, 0 };
	REQUIRE(mprotect(mapping, (size_t)page, PROT_READ) == 0);
	REQUIRE(syscall(SYS_capget, readonly, data) == 0);
	REQUIRE(memcmp(original, data, sizeof(data)) == 0);
	REQUIRE(syscall(SYS_capset, readonly, original) == 0);
	ERROR(syscall(SYS_capget, split, NULL), EFAULT);
	ERROR(syscall(SYS_capset, split, original), EFAULT);
	REQUIRE(mprotect(mapping, (size_t)page, PROT_READ | PROT_WRITE) == 0);
	header = (struct cap_header){ VERSION_3, 0 };
	struct cap_data *partial = (void *)(mapping + page - sizeof(*partial));
	memcpy(partial, original, sizeof(*partial));
	ERROR(syscall(SYS_capset, &header, partial), EFAULT);
	ERROR(syscall(SYS_capget, &header, partial), EFAULT);
	REQUIRE(syscall(SYS_capget, &header, data) == 0);
	REQUIRE(memcmp(original, data, sizeof(data)) == 0);

	memcpy(data, original, sizeof(data));
	data[0].effective = 1;
	data[0].permitted = 0;
	ERROR(syscall(SYS_capset, &header, data), EPERM);
	REQUIRE(syscall(SYS_capget, &header, data) == 0);
	REQUIRE(memcmp(original, data, sizeof(data)) == 0);
	data[1].effective |= UINT32_C(0x80000000);
	data[1].permitted |= UINT32_C(0x80000000);
	data[1].inheritable |= UINT32_C(0x80000000);
	REQUIRE(syscall(SYS_capset, &header, data) == 0);
	REQUIRE(syscall(SYS_capget, &header, data) == 0);
	REQUIRE(memcmp(original, data, sizeof(data)) == 0);

	memcpy(partial, original, sizeof(*partial));
	header.version = VERSION_1;
	REQUIRE(syscall(SYS_capget, &header, partial) == 0);
	REQUIRE(syscall(SYS_capset, &header, partial) == 0);
	header.version = VERSION_3;
	REQUIRE(syscall(SYS_capget, &header, data) == 0);
	REQUIRE(memcmp(&original[0], &data[0], sizeof(data[0])) == 0);
	REQUIRE(data[1].effective == 0 && data[1].permitted == 0 &&
		data[1].inheritable == 0);
	REQUIRE(munmap(mapping, (size_t)page * 2) == 0);
	puts("CAPABILITY_ABI versions=1,2,3; split/readonly/fault precedence; unknown bits masked; legacy upper clear");
	return 0;
}
