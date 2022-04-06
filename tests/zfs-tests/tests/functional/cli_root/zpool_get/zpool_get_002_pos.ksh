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
# Copyright 2008 Sun Microsystems, Inc.  All rights reserved.
# Use is subject to license terms.
#

#
# Copyright (c) 2013, 2021 by Delphix. All rights reserved.
#

. $STF_SUITE/include/libtest.shlib
. $STF_SUITE/tests/functional/cli_root/zpool_get/zpool_get.cfg

#
# DESCRIPTION:
#
# zpool get all works as expected
#
# STRATEGY:
#
# 1. Using zpool get, retrieve all default values
# 2. Verify that the header is printed
# 3. Get each property individually
#
# Test for those properties are expected to check whether their
# default values are sane, or whether they can be changed with zpool set.
#

log_assert "Zpool get all works as expected"
log_onexit cleanup

if ! is_global_zone ; then
	TESTPOOL=${TESTPOOL%%/*}
fi

typeset tmpfile=$(mktemp)

log_must eval "zpool get all $TESTPOOL >$tmpfile"
log_must grep -q "^NAME " $tmpfile

for prop in $(get_pool_props); do
	log_must eval "zpool get "$prop" $TESTPOOL"
	log_must grep -q "$TESTPOOL *$prop" $tmpfile
done

log_pass "Zpool get all works as expected"
