#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

set -e

./mmap/data_account
./mmap/grow_down
./mmap/mmap_and_fork
./mmap/mmap_and_mprotect
./mmap/mmap_and_mremap
./mmap/mmap_beyond_the_file
./mmap/mmap_err
./mmap/mmap_holes
./mmap/mmap_readahead
./mmap/mmap_shared_filebacked
./mmap/mmap_vmrss
./mmap/oom_reserve
./mmap/rev_map_ext2
./mmap/rev_map_tmpfs
