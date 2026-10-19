/* SPDX-License-Identifier: MPL-2.0 */
#define _GNU_SOURCE
#include <assert.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <unistd.h>

int main(void)
{
	char name[] = "/tmp/proc_path_mode_XXXXXX";
	int original = mkstemp(name);
	assert(original >= 0);
	const int flags[] = { O_RDONLY, O_WRONLY, O_RDWR, O_PATH };
	const mode_t modes[] = { 0500, 0300, 0700, 0000 };
	for (unsigned int i = 0; i < sizeof(flags) / sizeof(flags[0]); ++i) {
		int fd = open(name, flags[i]);
		assert(fd >= 0);
		char path[80];
		assert(snprintf(path, sizeof(path), "/proc/self/fd/%d", fd) >
		       0);
		struct stat metadata;
		assert(lstat(path, &metadata) == 0);
		assert((metadata.st_mode & 0777) == modes[i]);
		assert(close(fd) == 0);
	}
	assert(close(original) == 0);
	assert(unlink(name) == 0);
	return 0;
}
