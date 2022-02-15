#!/bin/ksh

#
# This file and its contents are supplied under the terms of the
# Common Development and Distribution License ("CDDL"), version 1.0.
# You may only use this file in accordance with the terms of version
# 1.0 of the CDDL.
#
# A full copy of the text of the CDDL should have accompanied this
# source.  A copy of the CDDL is also available via the Internet at
# http://www.illumos.org/license/CDDL.
#

#
# Copyright (c) 2015, 2022 by Delphix. All rights reserved.
#

. $STF_SUITE/include/libtest.shlib
. $STF_SUITE/tests/perf/perf.shlib

function cleanup
{
	# kill fio and iostat
	pkill fio
	pkill iostat

	#
	# We're using many filesystems depending on the number of
	# threads for each test, and there's no good way to get a list
	# of all the filesystems that should be destroyed on cleanup
	# (i.e. the list of filesystems used for the last test ran).
	# Thus, we simply destroy the pool as a way to destroy all
	# filesystems.
	#
	destroy_perf_pool
}

trap "log_fail \"Measure IO stats during random write load\"" SIGTERM
log_onexit cleanup

recreate_perf_pool

# Aim to fill the pool to 50% capacity while accounting for a 3x compressratio.
if use_object_store; then
	export TOTAL_SIZE=$((128 * 1024 * 1024 * 1024))
else
	export TOTAL_SIZE=$(($(get_prop avail $PERFPOOL) * 3 / 2))
fi

# Variables specific to this test for use by fio.
export PERF_NTHREADS=${PERF_NTHREADS:-'1 16 64'}
export PERF_NTHREADS_PER_FS=${PERF_NTHREADS_PER_FS:-'0 1'}
export PERF_IOSIZES=${PERF_IOSIZES:-'8k'}
export PERF_SYNC_TYPES=${PERF_SYNC_TYPES:-'1'}

# Until the performance tests over NFS can deal with multiple file systems,
# force the use of only one file system when testing over NFS.
[[ $NFS -eq 1 ]] && PERF_NTHREADS_PER_FS='0'

log_note \
    "ZIL specific random write workload with settings: $(print_perf_settings)"
do_fio_run random_writes.fio true false
log_pass "Measure IO stats during ZIL specific random write workload"
