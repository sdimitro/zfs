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

#
# Description:
# Measure file deletion operation, i.e. rm(1) command.
#
# Prior to deletion, the dataset is created and fio randomly writes a new
# file into an otherwise empty pool.
#

. $STF_SUITE/include/libtest.shlib
. $STF_SUITE/tests/perf/perf.shlib

function cleanup
{
	# kill fio and iostat
	pkill fio
	pkill iostat
	destroy_perf_pool
}

trap "log_fail \"Measure stats during file deletion\"" SIGTERM
log_onexit cleanup

recreate_perf_pool
populate_perf_filesystems

#
# AWS VM has a 500GB EBS storage limit so 400GB pool (70GB rpool disk) is the expected
# pool size.  With 3x compressratio and 50% fill target, file size is ~600GB.
#
if use_object_store; then
	export TOTAL_SIZE=$((600 * 1024 * 1024 * 1024))
else
	export TOTAL_SIZE=$(($(get_prop avail $PERFPOOL) * 3 / 2))
fi

# Variables for use by fio.
export PERF_NTHREADS=1
export PERF_NTHREADS_PER_FS=${PERF_NTHREADS_PER_FS:-'0'}
export PERF_SYNC_TYPES=${PERF_SYNC_TYPES:-'0'}
export PERF_IOSIZES=${PERF_IOSIZES:-'8k'}
export PERF_NUMIOS=655360 # 5GB worth of IOs

# Random writing to the file
export NUMJOBS=$(get_max $PERF_NTHREADS)
export FILE_SIZE=$((TOTAL_SIZE / NUMJOBS))
export DIRECTORY=$(get_directory)
export SYNC_TYPE=$PERF_SYNC_TYPES

log_note "Random writes"
log_must fio $FIO_SCRIPTS/random_writes_fill.fio
log_must zpool sync $PERFPOOL
log_must zinject -a

# Run log collection for only 10 seconds which should be sufficient.
export PERF_RUNTIME=10
do_collect_scripts delete

log_note "Removing file"
directory=$(get_directory)
log_note "DIRECTORY: " $directory
for f in $(ls $directory); do
	typeset t0=$SECONDS
	log_must rm ${directory}/${f}
	typeset elapsed=$((SECONDS - t0))
	log_note "${directory}/${f} deletion took: ${elapsed} secs"
done
log_pass "Measure stats during file deletion"
