// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <signal.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define COPY(in, inoff, out, outoff, len, flags) \
	syscall(SYS_copy_file_range, in, inoff, out, outoff, len, flags)
#define DATA_SIZE 8193

static volatile sig_atomic_t file_size_signals;

static void file_size_signal(int number)
{
	assert(number == SIGXFSZ);
	file_size_signals++;
}

static void expect_error(long result, int error)
{
	assert(result == -1 && errno == error);
}

static void check_offsets(int input, int output, const char *data)
{
	for (int explicit_input = 0; explicit_input < 2; explicit_input++) {
		for (int explicit_output = 0; explicit_output < 2;
		     explicit_output++) {
			assert(ftruncate(output, 0) == 0);
			assert(lseek(input, explicit_input ? 11 : 2,
				     SEEK_SET) >= 0);
			assert(lseek(output, explicit_output ? 13 : 3,
				     SEEK_SET) >= 0);
			off_t input_offset = 2, output_offset = 3;
			off_t *in_pointer = explicit_input ? &input_offset :
							     NULL;
			off_t *out_pointer = explicit_output ? &output_offset :
							       NULL;
			assert(COPY(input, in_pointer, output, out_pointer,
				    DATA_SIZE, 0) == DATA_SIZE - 2);
			assert(lseek(input, 0, SEEK_CUR) ==
			       (explicit_input ? 11 : DATA_SIZE));
			assert(lseek(output, 0, SEEK_CUR) ==
			       (explicit_output ? 13 : DATA_SIZE + 1));
			assert(input_offset ==
			       (explicit_input ? DATA_SIZE : 2));
			assert(output_offset ==
			       (explicit_output ? DATA_SIZE + 1 : 3));
			char copied[DATA_SIZE + 1];
			assert(pread(output, copied, sizeof(copied), 0) ==
			       sizeof(copied));
			assert(copied[0] == 0 && copied[1] == 0 &&
			       copied[2] == 0);
			assert(memcmp(copied + 3, data + 2, DATA_SIZE - 2) ==
			       0);
			assert(COPY(input, in_pointer, output, out_pointer, 1,
				    0) == 0);
		}
	}
}

static void check_copyout_fault(int input, int output)
{
	long page = sysconf(_SC_PAGESIZE);
	off_t *read_only = mmap(NULL, page, PROT_READ | PROT_WRITE,
				MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	assert(read_only != MAP_FAILED);
	*read_only = 0;
	assert(mprotect(read_only, page, PROT_READ) == 0);
	assert(lseek(input, 7, SEEK_SET) == 7);
	assert(lseek(output, 9, SEEK_SET) == 9);
	off_t output_offset = 0;
	assert(COPY(input, read_only, output, &output_offset, 0, 0) == 0);
	expect_error(COPY(input, read_only, output, &output_offset, 2, 0),
		     EFAULT);
	assert(*read_only == 0 && output_offset == 2);
	assert(lseek(input, 0, SEEK_CUR) == 7);
	assert(lseek(output, 0, SEEK_CUR) == 9);
	expect_error(COPY(input, read_only, output, NULL, 2, 0), EFAULT);
	assert(lseek(output, 0, SEEK_CUR) == 11);
	assert(munmap(read_only, page) == 0);
}

static void check_file_size_limit(int input, int output)
{
	pid_t child = fork();
	assert(child >= 0);
	if (child == 0) {
		assert(signal(SIGXFSZ, file_size_signal) != SIG_ERR);
		struct rlimit limit;
		assert(getrlimit(RLIMIT_FSIZE, &limit) == 0);
		limit.rlim_cur = 2;
		assert(setrlimit(RLIMIT_FSIZE, &limit) == 0);
		off_t input_offset = 0, output_offset = 0;
		assert(COPY(input, &input_offset, output, &output_offset, 8,
			    0) == 2);
		assert(input_offset == 2 && output_offset == 2);
		expect_error(COPY(input, &input_offset, output, &output_offset,
				  1, 0),
			     EFBIG);
		assert(file_size_signals == 1);
		expect_error(COPY(input, &input_offset, output, &output_offset,
				  0, 0),
			     EFBIG);
		assert(file_size_signals == 2);
		_exit(0);
	}
	int status;
	assert(waitpid(child, &status, 0) == child);
	assert(WIFEXITED(status) && WEXITSTATUS(status) == 0);
}

int main(void)
{
	char input_name[] = "/tmp/copy_input_XXXXXX";
	char output_name[] = "/tmp/copy_output_XXXXXX";
	int input = mkstemp(input_name), output = mkstemp(output_name);
	assert(input >= 0 && output >= 0);
	char data[DATA_SIZE];
	for (size_t i = 0; i < sizeof(data); i++)
		data[i] = (char)(i * 37);
	assert(write(input, data, sizeof(data)) == sizeof(data));
	check_offsets(input, output, data);
	check_copyout_fault(input, output);
	check_file_size_limit(input, output);

	off_t offset = 0;
	off_t *bad = (off_t *)(uintptr_t)1;
	expect_error(COPY(-1, bad, output, NULL, 0, 1), EBADF);
	expect_error(COPY(input, bad, output, NULL, 0, 1), EFAULT);
	expect_error(COPY(input, NULL, output, NULL, 0, 1), EINVAL);
	offset = -1;
	expect_error(COPY(input, &offset, output, NULL, 0, 0), EINVAL);
	expect_error(COPY(input, &offset, output, NULL, 1, 0), EOVERFLOW);
	offset = 1;
	expect_error(COPY(input, &offset, output, NULL, SIZE_MAX, 0),
		     EOVERFLOW);
	offset = LLONG_MAX;
	assert(COPY(input, &offset, output, NULL, 0, 0) == 0);
	offset = 0;
	off_t destination = LLONG_MAX;
	expect_error(COPY(input, &offset, output, &destination, 0, 0), EFBIG);
	destination = 1;
	expect_error(COPY(input, &offset, input, &destination, 2, 0), EINVAL);
	destination = DATA_SIZE;
	assert(COPY(input, &offset, input, &destination, 2, 0) == 2);
	int path = open(input_name, O_PATH);
	assert(path >= 0);
	expect_error(COPY(path, NULL, output, NULL, 0, 0), EBADF);
	assert(close(path) == 0);
	int directory = open("/tmp", O_RDONLY | O_DIRECTORY);
	assert(directory >= 0);
	expect_error(COPY(directory, NULL, output, NULL, 0, 0), EISDIR);
	assert(close(directory) == 0);
	int device = open("/dev/null", O_RDONLY);
	assert(device >= 0);
	expect_error(COPY(device, NULL, output, NULL, 0, 0), EINVAL);
	assert(close(device) == 0);
	int read_only = open(output_name, O_RDONLY);
	int write_only = open(input_name, O_WRONLY);
	assert(read_only >= 0 && write_only >= 0);
	expect_error(COPY(input, NULL, read_only, NULL, 0, 0), EBADF);
	expect_error(COPY(write_only, NULL, output, NULL, 0, 0), EBADF);
	assert(close(read_only) == 0 && close(write_only) == 0);
	assert(fcntl(output, F_SETFL, O_APPEND) == 0);
	expect_error(COPY(input, NULL, output, NULL, 0, 0), EBADF);
	assert(close(input) == 0 && close(output) == 0);
	assert(unlink(input_name) == 0 && unlink(output_name) == 0);
	return 0;
}
