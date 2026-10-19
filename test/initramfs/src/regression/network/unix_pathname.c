// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <stddef.h>
#include <fcntl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

#include "../common/test.h"

FN_TEST(unix_bind_applies_umask)
{
	struct sockaddr_un address = { .sun_family = AF_UNIX };
	snprintf(address.sun_path, sizeof(address.sun_path),
		 "/tmp/unix-permissions-%d", getpid());
	mode_t previous = umask(0077);
	int listener = TEST_SUCC(socket(AF_UNIX, SOCK_STREAM, 0));
	TEST_SUCC(bind(listener, (struct sockaddr *)&address, sizeof(address)));
	umask(previous);
	struct stat metadata;
	TEST_RES(stat(address.sun_path, &metadata),
		 _ret == 0 && S_ISSOCK(metadata.st_mode) &&
			 (metadata.st_mode & 0777) == 0700 &&
			 metadata.st_uid == getuid() &&
			 metadata.st_gid == getgid());
	TEST_ERRNO(open(address.sun_path, O_RDONLY), ENXIO);
	int path = TEST_SUCC(open(address.sun_path, O_PATH));
	TEST_RES(fstat(path, &metadata),
		 _ret == 0 && S_ISSOCK(metadata.st_mode));
	TEST_SUCC(close(path));
	TEST_SUCC(listen(listener, 1));
	TEST_SUCC(chmod(address.sun_path, 0222));
	int client = TEST_SUCC(socket(AF_UNIX, SOCK_STREAM, 0));
	TEST_SUCC(
		connect(client, (struct sockaddr *)&address, sizeof(address)));
	int accepted = TEST_SUCC(accept(listener, NULL, NULL));
	TEST_SUCC(close(accepted));
	TEST_SUCC(close(client));
	TEST_SUCC(close(listener));
	TEST_SUCC(unlink(address.sun_path));
}
END_TEST()

FN_TEST(unix_connect_needs_only_write_permission)
{
	struct sockaddr_un address = { .sun_family = AF_UNIX };
	snprintf(address.sun_path, sizeof(address.sun_path),
		 "/tmp/unix-write-only-%d", getpid());
	int listener = CHECK(socket(AF_UNIX, SOCK_STREAM, 0));
	CHECK(bind(listener, (struct sockaddr *)&address, sizeof(address)));
	CHECK(chmod(address.sun_path, 0222));
	CHECK(listen(listener, 1));
	pid_t child = CHECK(fork());
	if (child == 0) {
		if (setgid(65534) || setuid(65534))
			_exit(1);
		int client = socket(AF_UNIX, SOCK_STREAM, 0);
		if (client < 0)
			_exit(1);
		int result = connect(client, (struct sockaddr *)&address,
				     sizeof(address));
		close(client);
		_exit(result ? 1 : 0);
	}
	int status;
	TEST_RES(waitpid(child, &status, 0), _ret == child &&
						     WIFEXITED(status) &&
						     WEXITSTATUS(status) == 0);
	TEST_SUCC(close(listener));
	TEST_SUCC(unlink(address.sun_path));
}
END_TEST()
