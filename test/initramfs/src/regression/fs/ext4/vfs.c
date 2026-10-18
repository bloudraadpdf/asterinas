// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <assert.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/fs.h>
#include <sys/ioctl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <sys/xattr.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/sysmacros.h>
#include <sys/vfs.h>
#include <unistd.h>

static void indexed_path(char *path, size_t capacity, unsigned index)
{
	char name[256];
	memset(name, 'z', 255);
	int prefix = snprintf(name, 20, "item%03u-", index);
	name[prefix] = 'z';
	name[255] = 0;
	assert(snprintf(path, capacity, "/tmp/ext4-vfs/indexed/%s", name) <
	       capacity);
}

int main(int argc, char **argv)
{
	if (argc != 2) {
		fprintf(stderr, "usage: %s EMPTY_EXT4_BLOCK_DEVICE\n", argv[0]);
		return 2;
	}
	assert(mkdir("/tmp/ext4-vfs", 0755) == 0);
	assert(mount(argv[1], "/tmp/ext4-vfs", "ext4", 0, NULL) == 0);
	struct statfs geometry;
	assert(statfs("/tmp/ext4-vfs", &geometry) == 0 &&
	       geometry.f_bavail > 0 && geometry.f_files > 0);
	assert(geometry.f_bsize == 1024 || geometry.f_bsize == 2048 ||
	       geometry.f_bsize == 4096);
	puts("EXT4_MOUNT_PASSED");
	int fd = open("/tmp/ext4-vfs/small", O_CREAT | O_EXCL | O_RDWR, 0600);
	assert(fd >= 0);
	const char payload[] = "seventeen-bytes!!";
	assert(sizeof(payload) - 1 == 17);
	assert(write(fd, payload, 17) == 17);
	assert(fsync(fd) == 0);
	struct stat status;
	assert(fstat(fd, &status) == 0 && status.st_size == 17);
	char bytes[8192];
	memset(bytes, 0, sizeof(bytes));
	assert(pread(fd, bytes, sizeof(bytes), 0) == 17 &&
	       !memcmp(bytes, payload, 17));
	assert(mkdir("/tmp/ext4-vfs-second", 0755) == 0);
#ifdef __asterinas__
	errno = 0;
	assert(mount(argv[1], "/tmp/ext4-vfs-second", "ext4", MS_SYNCHRONOUS,
		     NULL) == -1 &&
	       errno == EOPNOTSUPP);
	errno = 0;
	assert(mount(argv[1], "/tmp/ext4-vfs-second", "ext4", 0, "unknown") ==
		       -1 &&
	       errno == EINVAL);
	errno = 0;
	assert(mount(argv[1], "/tmp/ext4-vfs-second", "ext4", MS_RDONLY,
		     NULL) == -1 &&
	       errno == EBUSY);
	errno = 0;
	assert(mount(argv[1], "/tmp/ext4-vfs-second", "ext4", MS_DIRSYNC,
		     NULL) == -1 &&
	       errno == EBUSY);
#endif
	assert(mount(argv[1], "/tmp/ext4-vfs-second", "ext4", 0, NULL) == 0);
	int second = open("/tmp/ext4-vfs-second/small", O_RDWR);
	assert(second >= 0);
	assert(pwrite(second, "A", 1, 3) == 1);
	assert(pread(fd, bytes, 17, 0) == 17 && bytes[3] == 'A');
	assert(close(second) == 0);
	assert(umount("/tmp/ext4-vfs-second") == 0 &&
	       rmdir("/tmp/ext4-vfs-second") == 0);
	assert(fsync(fd) == 0);
	assert(fstat(fd, &status) == 0 && status.st_size == 17);
	unsigned char *fault = mmap(NULL, 8192, PROT_READ | PROT_WRITE,
				    MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	assert(fault != MAP_FAILED);
	memset(fault, 0x7a, 4096);
	assert(mprotect(fault + 4096, 4096, PROT_NONE) == 0);
	errno = 0;
	assert(pwrite(fd, fault + 4096, 1, 8192) == -1 && errno == EFAULT);
	assert(fstat(fd, &status) == 0 && status.st_size == 17);
	assert(pwrite(fd, fault + 4080, 32, 17) == 16);
	assert(fstat(fd, &status) == 0 && status.st_size == 33);
	assert(ftruncate(fd, 17) == 0 && munmap(fault, 8192) == 0);
	puts("EXT4_USER_COPY_FAULT_PASSED");
	puts("EXT4_EOF_IDENTITY_PASSED");
	assert(ftruncate(fd, sizeof(bytes)) == 0);
	unsigned char *map = mmap(NULL, sizeof(bytes), PROT_READ | PROT_WRITE,
				  MAP_SHARED, fd, 0);
	assert(map != MAP_FAILED);
	map[18] = 0x57;
	map[4099] = 0x98;
	assert(fsync(fd) == 0);
	assert(pread(fd, bytes, sizeof(bytes), 0) == sizeof(bytes));
	assert((unsigned char)bytes[18] == 0x57 &&
	       (unsigned char)bytes[4099] == 0x98);
	assert(munmap(map, sizeof(bytes)) == 0);
	assert(ftruncate(fd, 11) == 0);
	assert(fsync(fd) == 0);
	assert(ftruncate(fd, sizeof(bytes)) == 0);
	assert(pread(fd, bytes, sizeof(bytes), 0) == sizeof(bytes));
	for (size_t i = 11; i < sizeof(bytes); ++i)
		assert(bytes[i] == 0);
	assert(fsync(fd) == 0);
	puts("EXT4_MAPPING_TRUNCATE_PASSED");
	assert(fsetxattr(fd, "user.probe", "value", 5, XATTR_CREATE) == 0);
	assert(fgetxattr(fd, "user.probe", bytes, sizeof(bytes)) == 5 &&
	       !memcmp(bytes, "value", 5));
	assert(fchmod(fd, 0640) == 0);
	assert(fstat(fd, &status) == 0 && (status.st_mode & 0777) == 0640);
	assert(mkdir("/tmp/ext4-vfs/dir", 0750) == 0);
	assert(link("/tmp/ext4-vfs/small", "/tmp/ext4-vfs/dir/link") == 0);
	assert(rename("/tmp/ext4-vfs/dir/link", "/tmp/ext4-vfs/linked") == 0);
	assert(unlink("/tmp/ext4-vfs/small") == 0);
	assert(unlink("/tmp/ext4-vfs/linked") == 0);
	assert(fstat(fd, &status) == 0 && status.st_nlink == 0);
	assert(pwrite(fd, "open orphan", 11, 0) == 11);
	assert(fsync(fd) == 0);
	second = open("/tmp/ext4-vfs/small", O_CREAT | O_EXCL | O_RDWR, 0600);
	assert(second >= 0);
	assert(write(second, payload, 17) == 17);
	assert(fsync(second) == 0);
	assert(pread(fd, bytes, 11, 0) == 11 &&
	       !memcmp(bytes, "open orphan", 11));
	assert(close(fd) == 0);
	assert(fgetxattr(second, "user.probe", bytes, sizeof(bytes)) == -1 &&
	       errno == ENODATA);
	assert(fsetxattr(second, "user.probe", "persisted", 9, 0) == 0);
	assert(fchmod(second, 0640) == 0);
	struct timespec times[2] = { { 1610000000, 123456789 },
				     { 1610000001, 987654321 } };
	assert(futimens(second, times) == 0);
	assert(close(second) == 0);
	int temporary =
		open("/tmp/ext4-vfs", O_TMPFILE | O_RDWR | O_EXCL, 0600);
	assert(temporary >= 0 && write(temporary, payload, 17) == 17 &&
	       fsync(temporary) == 0);
	assert(fstat(temporary, &status) == 0 && status.st_nlink == 0);
	assert(close(temporary) == 0);
	assert(symlink("small", "/tmp/ext4-vfs/symlink") == 0);
	assert(readlink("/tmp/ext4-vfs/symlink", bytes, sizeof(bytes)) == 5 &&
	       !memcmp(bytes, "small", 5));
	assert(mkfifo("/tmp/ext4-vfs/fifo", 0600) == 0);
	fd = open("/tmp/ext4-vfs/fifo", O_RDWR | O_NONBLOCK);
	assert(fd >= 0 && write(fd, "pipe", 4) == 4 &&
	       read(fd, bytes, 4) == 4 && !memcmp(bytes, "pipe", 4));
	assert(close(fd) == 0);
	assert(unlink("/tmp/ext4-vfs/fifo") == 0);
	assert(rmdir("/tmp/ext4-vfs/dir") == 0);
	int server = socket(AF_UNIX, SOCK_DGRAM, 0);
	int client = socket(AF_UNIX, SOCK_DGRAM, 0);
	assert(server >= 0 && client >= 0);
	struct sockaddr_un address = { .sun_family = AF_UNIX };
	strcpy(address.sun_path, "/tmp/ext4-vfs/socket");
	int barrier[2];
	assert(pipe(barrier) == 0);
	pid_t binders[2];
	for (unsigned i = 0; i < 2; ++i) {
		binders[i] = fork();
		assert(binders[i] >= 0);
		if (binders[i] == 0) {
			assert(close(barrier[1]) == 0);
			char go;
			assert(read(barrier[0], &go, 1) == 1);
			if (i == 1)
				strcpy(address.sun_path,
				       "/tmp/ext4-vfs/socket-other");
			int result = bind(server, (struct sockaddr *)&address,
					  sizeof(address));
			_exit(result == 0 ? 10 : errno == EINVAL ? 20 : 30);
		}
	}
	assert(close(barrier[0]) == 0 && write(barrier[1], "xx", 2) == 2 &&
	       close(barrier[1]) == 0);
	int results[2];
	for (unsigned i = 0; i < 2; ++i) {
		int result;
		assert(waitpid(binders[i], &result, 0) == binders[i] &&
		       WIFEXITED(result));
		results[i] = WEXITSTATUS(result);
	}
	assert((results[0] == 10 && results[1] == 20) ||
	       (results[0] == 20 && results[1] == 10));
	if (results[1] == 10)
		strcpy(address.sun_path, "/tmp/ext4-vfs/socket-other");
	const char *unused = results[0] == 10 ? "/tmp/ext4-vfs/socket-other" :
						"/tmp/ext4-vfs/socket";
	errno = 0;
	assert(lstat(unused, &status) == -1 && errno == ENOENT);
	assert(sendto(client, "unix", 4, 0, (struct sockaddr *)&address,
		      sizeof(address)) == 4);
	assert(recv(server, bytes, sizeof(bytes), 0) == 4 &&
	       !memcmp(bytes, "unix", 4));
	assert(unlink(address.sun_path) == 0 && close(client) == 0 &&
	       close(server) == 0);
	assert(mknod("/tmp/ext4-vfs/null", S_IFCHR | 0600, makedev(1, 3)) == 0);
	int device = open("/tmp/ext4-vfs/null", O_RDWR);
	assert(device >= 0 && write(device, "x", 1) == 1 &&
	       read(device, bytes, 1) == 0);
	assert(fstat(device, &status) == 0 && status.st_rdev == makedev(1, 3));
	assert(close(device) == 0 && unlink("/tmp/ext4-vfs/null") == 0);
	assert(mknod("/tmp/ext4-vfs/block", S_IFBLK | 0600,
		     makedev(259, 703710)) == 0);
	assert(stat("/tmp/ext4-vfs/block", &status) == 0 &&
	       S_ISBLK(status.st_mode) &&
	       status.st_rdev == makedev(259, 703710));
	assert(unlink("/tmp/ext4-vfs/block") == 0);
	puts("EXT4_SPECIAL_FILES_PASSED");
	puts("EXT4_NAMESPACE_ORPHAN_PASSED");
	assert(mkdir("/tmp/ext4-vfs/indexed", 0755) == 0);
	char path[512];
	for (unsigned i = 0; i < 100; ++i) {
		indexed_path(path, sizeof(path), i);
		int item = open(path, O_CREAT | O_EXCL | O_RDWR, 0600);
		assert(item >= 0 && close(item) == 0);
	}
	DIR *directory = opendir("/tmp/ext4-vfs/indexed");
	assert(directory);
	for (unsigned pass = 0; pass < 2; ++pass) {
		unsigned char seen[100] = { 0 };
		unsigned count = 0;
		struct dirent *entry;
		errno = 0;
		while ((entry = readdir(directory)) != NULL) {
			if (!strcmp(entry->d_name, ".") ||
			    !strcmp(entry->d_name, ".."))
				continue;
			unsigned index = 100;
			assert(!memcmp(entry->d_name, "item", 4) &&
			       sscanf(entry->d_name + 4, "%3u", &index) == 1);
			assert(index < 100 && seen[index] == 0);
			seen[index] = 1;
			++count;
		}
		assert(errno == 0 && count == 100);
		rewinddir(directory);
	}
	assert(closedir(directory) == 0);
	for (unsigned i = 0; i < 100; ++i) {
		indexed_path(path, sizeof(path), i);
		assert(unlink(path) == 0);
	}
	assert(rmdir("/tmp/ext4-vfs/indexed") == 0);
	puts("EXT4_INDEXED_READDIR_PASSED");

	int full = open("/tmp/ext4-vfs/full", O_CREAT | O_EXCL | O_RDWR, 0600);
	assert(full >= 0);
	unsigned char fill[4096];
	memset(fill, 0x57, sizeof(fill));
	int exhausted = 0;
	for (size_t i = 0; i < 20000; ++i) {
		ssize_t count = write(full, fill, sizeof(fill));
		if (count < 0) {
			assert(errno == ENOSPC);
			exhausted = 1;
			break;
		}
		assert(count == sizeof(fill));
		if (i % 256 == 255 && fsync(full) != 0) {
			assert(errno == ENOSPC);
			exhausted = 1;
			break;
		}
	}
	assert(exhausted);
	assert(unlink("/tmp/ext4-vfs/full") == 0 && close(full) == 0);
	full = open("/tmp/ext4-vfs/after-full", O_CREAT | O_EXCL | O_RDWR,
		    0600);
	assert(full >= 0 && write(full, payload, 17) == 17 && fsync(full) == 0);
	assert(close(full) == 0 && unlink("/tmp/ext4-vfs/after-full") == 0);
	puts("EXT4_NO_SPACE_RECLAIM_PASSED");
	fd = open("/tmp/ext4-vfs", O_RDONLY | O_DIRECTORY);
	assert(fd >= 0 && syncfs(fd) == 0 && close(fd) == 0);
	assert(umount("/tmp/ext4-vfs") == 0);
	int remount = mount(argv[1], "/tmp/ext4-vfs", "ext4", MS_RDONLY, NULL);
	if (remount != 0)
		perror("ext4 read-only remount");
	assert(remount == 0);
	fd = open("/tmp/ext4-vfs/small", O_RDONLY);
	assert(fd >= 0);
	assert(fstat(fd, &status) == 0 && status.st_size == 17 &&
	       (status.st_mode & 0777) == 0640);
	assert(status.st_mtim.tv_sec == 1610000001 &&
	       status.st_mtim.tv_nsec == 987654321);
	assert(syncfs(fd) == 0);
	assert(read(fd, bytes, sizeof(bytes)) == 17 &&
	       !memcmp(bytes, payload, 17));
	assert(fgetxattr(fd, "user.probe", bytes, sizeof(bytes)) == 9 &&
	       !memcmp(bytes, "persisted", 9));
	assert(close(fd) == 0);
	errno = 0;
	assert(open("/tmp/ext4-vfs/no", O_CREAT | O_WRONLY, 0600) == -1 &&
	       errno == EROFS);
	assert(umount("/tmp/ext4-vfs") == 0);
	puts("EXT4_CLEAN_REMOUNT_PASSED");
	assert(rmdir("/tmp/ext4-vfs") == 0);
	puts("EXT4_VFS_PROBE_PASSED");
	return 0;
}
