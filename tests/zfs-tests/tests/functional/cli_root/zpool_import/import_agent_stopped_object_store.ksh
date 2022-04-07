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
# Copyright 2007 Sun Microsystems, Inc.  All rights reserved.
# Use is subject to license terms.
#

#
# Copyright (c) 2022 by Delphix. All rights reserved.
#

. $STF_SUITE/include/libtest.shlib

#
# DESCRIPTION:
# Importing an exported pool when the zfs-object-agent is stopped
# should result in error
#
# STRATEGY:
#	1. Create and export the default test pool.
#	2. Stop the zfs object agent
#	3. Try to import the pool
#	4. Verify that the pool can not be imported as connection to
#	   object agent is closed
#	5. Start the object agent
#	6. Verify that the pool can be imported once the agent is up and running
#

verify_runnable "global"

if ! use_object_store; then
	log_unsupported "Not supported for block device based runs"
fi

function cleanup
{
	# Restart the zfs object agent to ensure it is
	# always up and running for next test
	restart_zfs_object_agent
}

log_onexit cleanup

log_assert "Verify that an exported pool cannot be imported when the " \
	"zfs object agent is not running"

log_must create_pool -p $TESTPOOL

# Write some data
log_must dd if=/dev/urandom of=/$TESTPOOL/datafile bs=1M count=100
log_must zpool sync

log_must zpool export $TESTPOOL

# Stop the object agent
log_must stop_zfs_object_agent

#
# This is a fault tolerant way to recover from issue
# where the import command may hang permanently when
# object agent is stopped and there exists multiple
# object store imported pools (May be from previous tests
# owing to any cleanup issue)

# We wait for atmost 2 mins and then restart the object
# agent in the background. This would cause the test to
# fail since we are trying to validate a negative assertion
# here(restarting will cause the import to success).
# But thats probabaly ok, rather can causing the system to
# hang forever.
#
sleep 120 && start_zfs_object_agent &
typeset pid=$!

log_note "Background process to start object agent in 2 mins started" \
	" with pid: $pid"

typeset OBJECT_STORE_PARAMS="-o object-endpoint=$ZTS_OBJECT_ENDPOINT \
    -o object-region=$ZTS_REGION \
    -o object-credentials-profile=${ZTS_CREDS_PROFILE:-default}"

# This should retry for 15 times before giving up

# Use the zpool import command directly instead of the
# library function import_pool since it makes use of
# zpool get command to check if pool exists and the
# command hangs if the object agent is not running
log_mustnot zpool import $OBJECT_STORE_PARAMS \
	-d $ZTS_BUCKET_NAME $TESTPOOL

#
# If the assertion was successfull, it means we need not to
# restart the object agent as the pool import didn't cause
# hang and errored out. Stop the background process as this
# would cause the object agent service to be restarted twice.
#
# In case of assertion failure, this would anyways return
# and proceed to cleanup.
#
kill -9 $pid

# Start the object agent
log_must start_zfs_object_agent

# Verify that the pool can be imported now
log_must import_pool -p "$TESTPOOL"

log_pass "Successfully verified that the pool import cannot be done when " \
	"object agent is not running"
