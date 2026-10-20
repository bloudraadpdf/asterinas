// SPDX-License-Identifier: MPL-2.0

#ifndef MLOCK_COMMON_H
#define MLOCK_COMMON_H

#include <assert.h>
#include <stdio.h>

static size_t locked_bytes(void)
{
	FILE *file = fopen("/proc/self/status", "r");
	assert(file != NULL);
	char line[256];
	size_t kilobytes;
	while (fgets(line, sizeof(line), file)) {
		if (sscanf(line, "VmLck: %zu kB", &kilobytes) == 1) {
			assert(fclose(file) == 0);
			return kilobytes * 1024;
		}
	}
	assert(0);
	return 0;
}

#endif
