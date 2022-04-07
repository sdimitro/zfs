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
# Test importing an exported pool with same name
#
# STRATEGY:
#	1. Create two test pools and export it. Both the pool name should be same
#	2. Try to import the pool which should fail
#	3. Verify that importing the first pool by guid works
#	4. Verify that the second exported pool can now be imported by its name
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

log_assert "Verify that the pool cannot be imported if multiple " \
	"exported pools exists with same name"

# Create the pool first time and export it
log_must create_pool -p $TESTPOOL1
# Store the pool guid for importing it via guid
typeset pool1_guid=$(get_config $TESTPOOL1 pool_guid)
log_must zpool export $TESTPOOL1

# Create the pool with same name and export it again
log_must create_pool -p $TESTPOOL1
# Save the guid
typeset pool2_guid=$(get_config $TESTPOOL1 pool_guid)
log_must zpool export $TESTPOOL1

# Importing pool should fail since there would be two pools with same name
log_mustnot import_pool -p "$TESTPOOL1"

# Try import using pool guid
log_must import_pool -p "$pool1_guid"

# Destroy the pool so that it can be imported using the same name
log_must destroy_pool $TESTPOOL1

# Verify that the second pool can be imported now using the name
log_must import_pool -p "$TESTPOOL1"

log_pass "Successfully verified that the pool cannot be imported if multiple "\
	"exported pools with the same name exist, unless imported by guid"
