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
#	Verify that the zfs-object-agent cannot be started with invalid zettacache
#	device name
#
# STRATEGY:
#	1. Get a list of devices that are available
#	2. Stop the zfs-object-agent and add the list of devices that were
#	   partitioned to be used as zettacache devices. Make sure we add only
#	   the basename of the device to ensure it is not correct
#	3. Start the zfs-object-agent
#	4. Verify from the object agent output that it is waiting for the devices to
#	   show up. It should eventually fail to start
#

verify_runnable "global"

if ! use_object_store; then
	log_unsupported "Test not applicable for block based run"
fi

function cleanup
{
	poolexists $TESTPOOL && destroy_pool $TESTPOOL
	invalidate_zcache
}

log_onexit cleanup

log_assert "Verify that the zfs object cannot be started with invalid zettacache" \
	" device names"

typeset invalid_devices=""

for device in $AVAILABLE_DEVICES; do
	invalid_devices+="/path/do/not/exist/$device "
done

log_note "Configuring zfs-object-agent with invalid devices [$invalid_devices]"
invalidate_zcache && configure_zettacache $invalid_devices

log_must eval "sudo systemctl status zfs-object-agent | grep -q 'Waiting for'"

log_pass "zfs object failed to start with invalid zettacache devices"
