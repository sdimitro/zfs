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
# Importing a pool with object agent stopped, when there already
# exists a second pool should result in indefinite wait until the object
# agent is started again
#
# STRATEGY:
#	1. Create one additional pool along with the default POOL
#	2. Export the second pool
#	3. Stop the zfs object agent
#	4. Run a background process to start object agent after 1 min
#	4. Verify that the pool can be imported after the agent was restarted
#

verify_runnable "global"

if ! use_object_store; then
	log_unsupported "Not supported for block device based runs"
fi

function cleanup
{
	poolexists $TESTPOOL1 && destroy_pool $TESTPOOL1
}

log_onexit cleanup

log_assert "Verify that the pool import hangs if multiple pool exists " \
	"and the zfs object agent is not running"

log_must create_pool -p $TESTPOOL1

# Write some data
log_must dd if=/dev/urandom of=/$TESTPOOL1/datafile bs=1M count=100
log_must zpool sync

log_must zpool export $TESTPOOL1

# Stop the object agent
log_must stop_zfs_object_agent

# Sleep for sometime and restart object agent in background
sleep 60 && start_zfs_object_agent &
typeset pid=$!

log_note "Started the zfs-object-agent in background with pid=$pid"


typeset object_store_params="-o object-endpoint=$ZTS_OBJECT_ENDPOINT
	-o object-region=$ZTS_REGION"

if [ $ZTS_OBJECT_STORE == "s3" ]; then
	if ! is_using_iam_role; then
		object_store_params="$object_store_params
			-o object-credentials-profile=${ZTS_CREDS_PROFILE:-default}"
	fi
elif [ $ZTS_OBJECT_STORE == "blob" ]; then
	object_store_params="-o object-protocol=blob"
fi

# The pool import should hang at this moment since there
# exists the default TESTPOOL. This import should resume once the object
# agent starts after a minute.
log_must zpool import $object_store_params \
	-d $ZTS_BUCKET_NAME $TESTPOOL1


log_pass "Successfully verified that the pool import command waits " \
	"until the object agent is restarted"
