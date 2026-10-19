/* SPDX-License-Identifier: MPL-2.0 */
#include <assert.h>

int main(void)
{
	volatile int value = 41;
	int nested(void)
	{
		return value + 1;
	}
	int (*volatile entry)(void) = nested;
	assert(entry() == 42);
	return 0;
}
