#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

set -e

./clktck

./clock_nanosleep/nanosleep_err

./gettimeofday/gettimeofday
./gettimeofday/realtime_epoch

./itimer/setitimer
./itimer/timer_create

./timerfd/timerfd_clockid
