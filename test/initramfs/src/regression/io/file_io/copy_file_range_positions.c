// SPDX-License-Identifier: MPL-2.0

#define _GNU_SOURCE
#include <assert.h>
#include <pthread.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#define COPY(in, inoff, out, outoff, len, flags) \
	syscall(SYS_copy_file_range, in, inoff, out, outoff, len, flags)

// Asterinas serialises copies using a shared open file description's position.
// This is a local contract check, separate from the Linux parity controls.
struct concurrent_copy {
	int input;
	int output;
	pthread_barrier_t *barrier;
};

static void *copy_records(void *argument)
{
	struct concurrent_copy *copy = argument;
	int result = pthread_barrier_wait(copy->barrier);
	assert(result == 0 || result == PTHREAD_BARRIER_SERIAL_THREAD);
	for (;;) {
		ssize_t copied =
			COPY(copy->input, NULL, copy->output, NULL, 64, 0);
		if (copied == 0)
			break;
		assert(copied == 64);
	}
	return NULL;
}

static void check_shared_positions(void)
{
	char names[3][40] = { "/tmp/copy_records_XXXXXX",
			      "/tmp/copy_first_XXXXXX",
			      "/tmp/copy_second_XXXXXX" };
	int files[3];
	for (int i = 0; i < 3; i++) {
		files[i] = mkstemp(names[i]);
		assert(files[i] >= 0 && unlink(names[i]) == 0);
	}
	char records[256][64] = { 0 };
	for (uint32_t i = 0; i < 256; i++)
		memcpy(records[i], &i, sizeof(i));
	assert(write(files[0], records, sizeof(records)) == sizeof(records));
	assert(lseek(files[0], 0, SEEK_SET) == 0);
	int duplicate = dup(files[0]);
	assert(duplicate >= 0);
	pthread_barrier_t barrier;
	assert(pthread_barrier_init(&barrier, NULL, 2) == 0);
	struct concurrent_copy copies[2] = { { files[0], files[1], &barrier },
					     { duplicate, files[2],
					       &barrier } };
	pthread_t threads[2];
	for (int i = 0; i < 2; i++)
		assert(pthread_create(&threads[i], NULL, copy_records,
				      &copies[i]) == 0);
	for (int i = 0; i < 2; i++)
		assert(pthread_join(threads[i], NULL) == 0);
	assert(pthread_barrier_destroy(&barrier) == 0);
	unsigned int seen[256] = { 0 };
	for (int i = 1; i < 3; i++) {
		assert(lseek(files[i], 0, SEEK_SET) == 0);
		char record[64];
		ssize_t size;
		while ((size = read(files[i], record, sizeof(record))) != 0) {
			assert(size == sizeof(record));
			uint32_t id;
			memcpy(&id, record, sizeof(id));
			assert(id < 256 && memcmp(record, records[id],
						  sizeof(record)) == 0);
			seen[id]++;
		}
	}
	for (int i = 0; i < 256; i++)
		assert(seen[i] == 1);
	assert(lseek(files[0], 0, SEEK_CUR) == sizeof(records));
	assert(close(duplicate) == 0);
	for (int i = 0; i < 3; i++)
		assert(close(files[i]) == 0);
}

int main(void)
{
	check_shared_positions();
	return 0;
}
