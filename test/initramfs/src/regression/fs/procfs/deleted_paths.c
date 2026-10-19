// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

static int check_link(int fd, const char *path, int deleted)
{
	char link[64], actual[512], expected[512];
	snprintf(link, sizeof(link), "/proc/self/fd/%d", fd);
	snprintf(expected, sizeof(expected), "%s%s", path,
		 deleted ? " (deleted)" : "");
	ssize_t length = readlink(link, actual, sizeof(actual) - 1);
	if (length < 0)
		return 1;
	actual[length] = 0;
	if (strcmp(actual, expected)) {
		fprintf(stderr, "link: expected %s, got %s\n", expected,
			actual);
		return 1;
	}
	return 0;
}

static int check_maps(const char *path)
{
	FILE *maps = fopen("/proc/self/maps", "r");
	if (!maps)
		return 1;
	char line[4096], expected[512];
	snprintf(expected, sizeof(expected), "%s (deleted)", path);
	int found = 0;
	while (fgets(line, sizeof(line), maps))
		if (strstr(line, expected))
			found = 1;
	int error = ferror(maps);
	if (fclose(maps))
		return 1;
	return error || !found;
}

int main(void)
{
	char source[] = "/tmp/proc-source-XXXXXX",
	     target[] = "/tmp/proc-target-XXXXXX";
	char other[512];
	int fd = mkstemp(source), replaced = mkstemp(target);
	if (fd < 0 || replaced < 0 || ftruncate(fd, 4096))
		return 1;
	void *mapping = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, fd, 0);
	if (mapping == MAP_FAILED)
		return 1;
	snprintf(other, sizeof(other), "%s-hardlink", source);
	if (link(source, other) || unlink(source) ||
	    check_link(fd, source, 1) || check_maps(source))
		return 1;
	if (unlink(other) || munmap(mapping, 4096) || close(fd))
		return 1;
	fd = open(source, O_CREAT | O_EXCL | O_RDWR, 0600);
	if (fd < 0 || rename(source, target) || check_link(fd, target, 0) ||
	    check_link(replaced, target, 1))
		return 1;
	return close(fd) || close(replaced) || unlink(target);
}
