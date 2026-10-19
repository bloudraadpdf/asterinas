# SPDX-License-Identifier: MPL-2.0

EXTRA_C_FLAGS := -static -lpthread

include ../../common/Makefile

$(OBJ_OUTPUT_DIR)/stack_exec: EXTRA_C_FLAGS += -Wl,-z,execstack
