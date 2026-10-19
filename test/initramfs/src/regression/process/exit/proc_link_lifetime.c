/* SPDX-License-Identifier: MPL-2.0 */
#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <dirent.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void)
{
	int channel[2];
	assert(pipe(channel) == 0);
	pid_t child = fork();
	assert(child >= 0);
	if (!child) {
		char byte;
		assert(close(channel[1]) == 0);
		assert(read(channel[0], &byte, 1) == 1);
		_exit(0);
	}
	char path[128], target[4096];
	assert(snprintf(path, sizeof(path), "/proc/%d/exe", child) > 0);
	int executable = open(path, O_PATH | O_NOFOLLOW);
	assert(snprintf(path, sizeof(path), "/proc/%d", child) > 0);
	int directory = open(path, O_RDONLY | O_DIRECTORY);
	assert(directory >= 0);
	assert(executable >= 0);
	assert(snprintf(path, sizeof(path), "/proc/%d/fd/%d", child,
			channel[0]) > 0);
	int descriptor = open(path, O_PATH | O_NOFOLLOW);
	assert(descriptor >= 0);
	assert(readlinkat(executable, "", target, sizeof(target)) > 0);
	assert(readlinkat(descriptor, "", target, sizeof(target)) > 0);
	assert(write(channel[1], "x", 1) == 1);
	int status;
	assert(waitpid(child, &status, 0) == child);
	assert(WIFEXITED(status) && WEXITSTATUS(status) == 0);
	errno = 0;
	assert(readlinkat(executable, "", target, sizeof(target)) == -1);
	assert(errno == ENOENT);
	errno = 0;
	assert(readlinkat(descriptor, "", target, sizeof(target)) == -1);
	assert(errno == ENOENT);
	DIR *stream = fdopendir(directory);
	assert(stream != NULL);
	errno = 0;
	assert(readdir(stream) == NULL && errno == 0);
	assert(closedir(stream) == 0);
	assert(close(executable) == 0);
	assert(close(descriptor) == 0);
	assert(close(channel[0]) == 0);
	assert(close(channel[1]) == 0);
	return 0;
}
