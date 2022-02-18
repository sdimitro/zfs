#!/bin/ksh -p
#
# CDDL HEADER START
#
# The contents of this file are subject to the terms of the
# Common Development and Distribution License (the "License").
# You may not use this file except in compliance with the License.
#
# You can obtain a copy of the license at usr/src/OPENSOLARIS.LICENSE
# or http://www.opensolaris.org/os/licensing.
# See the License for the specific language governing permissions
# and limitations under the License.
#
# When distributing Covered Code, include this CDDL HEADER in each
# file and include the License file at usr/src/OPENSOLARIS.LICENSE.
# If applicable, add the following below this CDDL HEADER, with the
# fields enclosed by brackets "[]" replaced with your own identifying
# information: Portions Copyright [yyyy] [name of copyright owner]
#
# CDDL HEADER END
#

#
# Copyright (c) 2022 by Delphix. All rights reserved.
#

. $STF_SUITE/include/libtest.shlib
. $STF_SUITE/include/object_store.shlib
. $STF_SUITE/tests/functional/slog/slog.kshlib
. $STF_SUITE/tests/functional/zcache_slog_sharing/zcache_slog_sharing.kshlib


#
# DESCRIPTION:
#	Verify that the shared device(s) are partitioned and partition 1 is log
#	device and partition 2 is the cache. The pool cannot be imported if the
#	slog device goes missing after the pool export
#
# STRATEGY:
#	1. Get a list of devices that are available
#	2. For each device partition it into two parts such that 1/4 of the
#	   disk size is allocated to the log device & remaining 3/4 to the
#	   zcache device
#	3. Stop the zfs-object-agent and add the list of devices that were
#	   partitioned to be used as zettacache devices
#	4. Start the zfs-object-agent
#	5. Create a pool with the log devices
#	6. Verify that the pool was created
#	7. Export the pool with the attached log devices
#	8. Destroy one or more slog devices
#	8. Import the pool and verify that the import cannot be done
#

verify_runnable "global"

if ! use_object_store; then
	log_unsupported "Test not applicable for block based run"
fi

function cleanup
{
	poolexists $TESTPOOL && destroy_pool $TESTPOOL
	invalidate_zettacache_devices $AVAILABLE_DEVICES
}

log_assert "Verify that the shared device(s) are partitioned into two parts." \
	" The first part should be usable as slog device and the second as zettacache" \
	" device. The pool cannot be imported back if one or more slog devices are missing"

log_onexit cleanup

log_must create_pool -p $TESTPOOL -l "log $SLOG_DEVICES"
log_must poolexists $TESTPOOL

log_must zfs create $TESTPOOL/$TESTFS

# Do some sync write
log_must dd if=/dev/zero of=/$TESTPOOL/$TESTFS/sync \
    conv=fdatasync,fsync bs=1M count=100

log_must zpool sync $TESTPOOL

verify_slog_devices_are_online $TESTPOOL $SLOG_DEVICES

log_must zpool export $TESTPOOL

reset_zcache_configuration

for dev in ${AVAILABLE_DEVICES}; do
	# Partition 1 is the slog part
	destroy_partition $dev 1
done

log_mustnot import_pool -p $TESTPOOL

log_pass "Partitioning the shared device and importing it fails if" \
	" the slog device used during export are missing"
