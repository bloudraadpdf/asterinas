#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

set -e

./pipe/pipe_err
./pipe/process_pipe_available
./pipe/short_rw

./sem/sem
./sem/control_lifetime

./msg/queue_budget

./shm/posix_shm
./shm/attachment_lifetime
