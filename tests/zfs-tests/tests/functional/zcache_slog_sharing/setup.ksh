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
. $STF_SUITE/tests/functional/zcache_slog_sharing/zcache_slog_sharing.kshlib

if ! use_object_store; then
	log_unsupported "Test not applicable for block based run"
fi

#
# Skip test for CI runs as we are not able to detect
# suitable devices to be used
#
# CI runs are returning a single device `sda` which
# seems to be inuse and can't be partitioned
#
if [ -n "$CI" ] && [ "$CI" == "true" ]; then
    log_unsupported "Test not supported for CI based run"
fi

if grep -q "/dev/loop" <<< "$AVAILABLE_DEVICES"; then
    log_unsupported "Test not applicable with loop devices"
fi

# Skip test if we are not able to find additional disks
# for the test
if [ -z "$AVAILABLE_DEVICES" ]; then
    log_unsupported "Test requires additional disks to be used as slog" \
        " and zettacache. None found"
fi

for device in $AVAILABLE_DEVICES; do
    log_note "Partitioning device: $device"
    log_must create_slog_zcache_partition $device
done


log_must configure_zettacache $CACHE_DEVICES
verify_zcache_devices_were_added $CACHE_DEVICES

log_note "Available devices [$AVAILABLE_DEVICES]"
log_note "Using slog devices [$SLOG_DEVICES]"
log_note "Using cache devices [$CACHE_DEVICES]"

log_pass
