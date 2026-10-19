// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

int main(void)
{
	int descriptors[140];
	for (unsigned i = 0; i < sizeof(descriptors) / sizeof(*descriptors);
	     ++i) {
		descriptors[i] = open("/dev/null", O_RDONLY);
		if (descriptors[i] < 0)
			return 1;
	}
	for (int pass = 0; pass < 2; ++pass) {
		DIR *directory = opendir("/proc/self/fd");
		if (!directory)
			return 1;
		unsigned found = 0;
		struct dirent *entry;
		errno = 0;
		while ((entry = readdir(directory))) {
			struct stat metadata;
			if (fstatat(dirfd(directory), entry->d_name, &metadata,
				    AT_SYMLINK_NOFOLLOW))
				return 1;
			if (entry->d_ino != metadata.st_ino) {
				fprintf(stderr,
					"%s: dirent inode %llu differs from stat %llu\n",
					entry->d_name,
					(unsigned long long)entry->d_ino,
					(unsigned long long)metadata.st_ino);
				return 1;
			}
			for (unsigned i = 0;
			     i < sizeof(descriptors) / sizeof(*descriptors);
			     ++i) {
				char name[24];
				snprintf(name, sizeof(name), "%d",
					 descriptors[i]);
				if (!strcmp(entry->d_name, name))
					++found;
			}
		}
		int error = errno;
		if (closedir(directory) || error ||
		    found != sizeof(descriptors) / sizeof(*descriptors))
			return 1;
	}
	for (unsigned i = 0; i < sizeof(descriptors) / sizeof(*descriptors);
	     ++i)
		if (close(descriptors[i]))
			return 1;
	return 0;
}
