/* SPDX-License-Identifier: MPL-2.0 */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stddef.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

static pthread_mutex_t mutex;

struct list_head {
	void *next;
	long offset;
	void *pending;
};

struct lock_entry {
	void *next;
	uint32_t word;
};

static struct list_head fault_head;
static struct lock_entry *fault_entry;
static size_t page_size;

static void *invalid_owner(void *mode)
{
	void *head = (void *)1;
	if (mode) {
		fault_entry->next = &fault_head;
		fault_entry->word = (uint32_t)syscall(SYS_gettid);
		fault_head = (struct list_head){
			.next = fault_entry,
			.offset = offsetof(struct lock_entry, word),
		};
		if (mprotect(fault_entry, page_size, PROT_READ) != 0)
			return (void *)1;
		head = &fault_head;
	}
	return (void *)(intptr_t)syscall(SYS_set_robust_list, head,
					 sizeof(fault_head));
}

static void *owner(void *unused)
{
	(void)unused;
	return (void *)(intptr_t)pthread_mutex_lock(&mutex);
}

int main(void)
{
	pthread_mutexattr_t attr;
	pthread_t thread;
	void *result;
	void *head;
	size_t length;
	if (syscall(SYS_get_robust_list, 0, &head, &length) != 0) {
		perror("get_robust_list");
		return 1;
	}
	if (length != 3 * sizeof(void *))
		return 1;
	errno = 0;
	if (syscall(SYS_set_robust_list, head, length - 1) != -1 ||
	    errno != EINVAL)
		return 1;
	errno = 0;
	if (syscall(SYS_get_robust_list, 0, NULL, &length) != -1 ||
	    errno != EFAULT)
		return 1;
	long size = sysconf(_SC_PAGESIZE);
	if (size <= 0)
		return 1;
	page_size = (size_t)size;
	fault_entry = mmap(NULL, page_size, PROT_READ | PROT_WRITE,
			   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	if (fault_entry == MAP_FAILED)
		return 1;
	for (intptr_t mode = 0; mode < 2; mode++) {
		if (pthread_create(&thread, NULL, invalid_owner,
				   (void *)mode) != 0)
			return 1;
		if (pthread_join(thread, &result) != 0 || result != NULL)
			return 1;
	}
	if (munmap(fault_entry, page_size) != 0)
		return 1;
	if (pthread_mutexattr_init(&attr) != 0)
		return 1;
	if (pthread_mutexattr_setrobust(&attr, PTHREAD_MUTEX_ROBUST) != 0)
		return 1;
	if (pthread_mutex_init(&mutex, &attr) != 0)
		return 1;
	if (pthread_mutexattr_destroy(&attr) != 0)
		return 1;
	if (pthread_create(&thread, NULL, owner, NULL) != 0)
		return 1;
	if (pthread_join(thread, &result) != 0 || result != NULL)
		return 1;
	int error = pthread_mutex_lock(&mutex);
	printf("ROBUST owner-death=%d expected=%d\n", error, EOWNERDEAD);
	if (error != EOWNERDEAD)
		return 1;
	if (pthread_mutex_consistent(&mutex) != 0)
		return 1;
	if (pthread_mutex_unlock(&mutex) != 0)
		return 1;
	if (pthread_mutex_lock(&mutex) != 0)
		return 1;
	if (pthread_mutex_unlock(&mutex) != 0)
		return 1;
	return pthread_mutex_destroy(&mutex) != 0;
}
