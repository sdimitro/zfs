/*
 * CDDL HEADER START
 *
 * The contents of this file are subject to the terms of the
 * Common Development and Distribution License (the "License").
 * You may not use this file except in compliance with the License.
 *
 * You can obtain a copy of the license at usr/src/OPENSOLARIS.LICENSE
 * or http://www.opensolaris.org/os/licensing.
 * See the License for the specific language governing permissions
 * and limitations under the License.
 *
 * When distributing Covered Code, include this CDDL HEADER in each
 * file and include the License file at usr/src/OPENSOLARIS.LICENSE.
 * If applicable, add the following below this CDDL HEADER, with the
 * fields enclosed by brackets "[]" replaced with your own identifying
 * information: Portions Copyright [yyyy] [name of copyright owner]
 *
 * CDDL HEADER END
 */
/*
 * Copyright (c) 2021, 2022 by Delphix. All rights reserved.
 */

#include <sys/stat.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <libintl.h>
#include <libzutil.h>
#include <libnvpair.h>
#include <sys/vdev_impl.h>
#include <sys/vdev_object_store.h>

#include "zutil_import.h"
#include "zutil_zoa.h"

/*
 * Number of times we try to connect to agent before failing.
 */
#define	ZOA_MAX_RETRIES	15

/*
 * This specifies that this code supports all 1.X.Y versions of the agent
 * communication protocol. This should be updated as new capabilities are
 * added and supported or required.
 */
#define	AGENT_PROTOCOL_VERSION "^1"

struct sockaddr_un zfs_public_socket = {
	AF_UNIX, "/etc/zfs/zfs_public_socket"
};

static struct sockaddr_un zfs_root_socket = {
	AF_UNIX, "/etc/zfs/zfs_root_socket"
};

static int
read_all(int fd, void *buf, size_t len)
{
	size_t read_total = 0;
	while (read_total < len) {
		int rc = read(fd, buf + read_total, len - read_total);
		if (rc > 0) {
			read_total += rc;
		} else if (rc < 0) {
			return (-errno);
		} else {
			return (-ENETDOWN);
		}
	}
	return (read_total);
}

static int
write_all(int fd, void *buf, size_t len)
{
	size_t write_total = 0;
	while (write_total < len) {
		int rc = write(fd, buf + write_total, len - write_total);
		if (rc > 0) {
			write_total += rc;
		} else if (rc < 0) {
			return (-errno);
		} else {
			return (-ENETDOWN);
		}
	}
	return (write_total);
}

static struct sockaddr *
get_zfs_socket(zoa_socket_t zoa_sock)
{
	switch (zoa_sock) {
		case ZFS_PUBLIC_SOCKET:
			return ((struct sockaddr *)&zfs_public_socket);
			break;
		case ZFS_ROOT_SOCKET:
			return ((struct sockaddr *)&zfs_root_socket);
			break;
		default:
			ASSERT(0);
			return (NULL);
	}
}

static nvlist_t *
zoa_send_recv_msg_impl(int sock, nvlist_t *msg, zoa_socket_t zoa_sock, int *err)
{
	size_t len;
	char *buf = fnvlist_pack(msg, &len);

	message_header_t header = {
		.message_type = MESSAGE_NVLIST,
		.struct_len = 0,
		.payload_len = len,
	};

	ssize_t rv = write_all(sock, &header, sizeof (header));
	if (rv < 0) {
		*err = rv;
		fnvlist_pack_free(buf, len);
		return (NULL);
	}
	ASSERT3U(rv, ==, sizeof (header));

	rv = write_all(sock, buf, len);
	fnvlist_pack_free(buf, len);
	if (rv < 0) {
		*err = rv;
		return (NULL);
	}
	VERIFY3U(rv, ==, len);

	rv = read_all(sock, &header, sizeof (header));
	if (rv < 0) {
		*err = rv;
		return (NULL);
	}
	VERIFY3U(rv, ==, sizeof (header));

	VERIFY3U(header.message_type, ==, MESSAGE_NVLIST);
	VERIFY0(header.struct_len);
	VERIFY3U(header.payload_len, <=, SPA_MAXBLOCKSIZE);

	buf = malloc(header.payload_len);
	rv = read_all(sock, buf, header.payload_len);
	if (rv < 0) {
		*err = rv;
		free(buf);
		return (NULL);
	}
	ASSERT3U(rv, ==, header.payload_len);

	nvlist_t *resp = fnvlist_unpack(buf, header.payload_len);
	free(buf);

	return (resp);
}

int
zoa_connect_agent(libpc_handle_t *hdl, zoa_socket_t zoa_sock,
    const char *version_req_str, nvlist_t **version)
{
	int sock;
	int retries = 0;

	for (;;) {
		sock = socket(AF_UNIX, SOCK_STREAM, 0);
		if (sock < 0) {
			zutil_error_aux(hdl, "%s", strerror(errno));
			zutil_error(hdl, EZFS_SOCKETFAILURE,
			    dgettext(TEXT_DOMAIN, "failed to create socket"));
			return (-1);
		}

		if (connect(sock, get_zfs_socket(zoa_sock),
		    sizeof (struct sockaddr_un)) == 0) {
			break;
		}

		/*
		 * If the object agent is running or has run in the past, the
		 * zoa socket file should be present. If this file is missing,
		 * it indicates that either the agent has never run or the
		 * superuser removed the file. Fail silently in this case.
		 */
		if (errno == ENOENT) {
			close(sock);
			return (-1);
		} else if (errno == ECONNREFUSED && retries < ZOA_MAX_RETRIES) {
			zutil_error(hdl, EZFS_CONNECT_RETRY,
			    dgettext(TEXT_DOMAIN,
			    "failed to connect to object agent process:"));
			retries++;
			sleep(1);
		} else {
			zutil_error_aux(hdl, "%s", strerror(errno));
			zutil_error(hdl, EZFS_CONNECT_REFUSED,
			    dgettext(TEXT_DOMAIN,
			    "connection to object agent process failed"));
			close(sock);
			return (-1);
		}
		close(sock);
	}
	nvlist_t *version_nvl = fnvlist_alloc();
	fnvlist_add_string(version_nvl, AGENT_REQUEST_TYPE, AGENT_TYPE_VERSION);
	fnvlist_add_string(version_nvl, AGENT_VERSION, version_req_str);
	int err;
	nvlist_t *resp = zoa_send_recv_msg_impl(sock, version_nvl, zoa_sock,
	    &err);
	fnvlist_free(version_nvl);
	if (resp == NULL) {
		zutil_error_aux(hdl, "%s", strerror(err));
		zutil_error(hdl, EZFS_CONNECT_REFUSED, dgettext(TEXT_DOMAIN,
		    "could not negotiate version with object agent process"));
		close(sock);
		return (-1);
	}
	if (version != NULL) {
		ASSERT0(strcmp(fnvlist_lookup_string(resp, AGENT_RESPONSE_TYPE),
		    AGENT_TYPE_VERSION));
		*version = fnvlist_dup(fnvlist_lookup_nvlist(resp,
		    AGENT_VERSION));
	}
	fnvlist_free(resp);
	return (sock);
}

nvlist_t *
zoa_send_recv_msg(libpc_handle_t *hdl, nvlist_t *msg,
    const char *version_req_str, zoa_socket_t zoa_sock)
{
	nvlist_t *resp = NULL;
	int retries = 0;
	for (; retries < ZOA_MAX_RETRIES; retries++) {
		int sock = zoa_connect_agent(hdl, zoa_sock, version_req_str,
		    NULL);
		if (sock == -1) {
			break;
		}

		int err = 0;
		resp = zoa_send_recv_msg_impl(sock, msg, ZFS_PUBLIC_SOCKET,
		    &err);
		close(sock);
		if (err == 0) {
			break;
		} else if (err != -ENETDOWN && err != -ECONNRESET) {
			zutil_error_fmt(hdl, EZFS_CONNECTION_ERROR,
			    dgettext(TEXT_DOMAIN,
			    "error communicating with object agent: %d"), err);
			break;
		}
		(void) zutil_error_fmt(hdl, EZFS_CONNECTION_ERROR,
		    dgettext(TEXT_DOMAIN,
		    "retrying on object agent error: %d"), err);
	}
	if (retries == ZOA_MAX_RETRIES) {
		(void) zutil_error_fmt(hdl, EZFS_CONNECTION_ERROR,
		    dgettext(TEXT_DOMAIN, "giving up after %d retries"),
		    retries);
	}

	fnvlist_free(msg);
	return (resp);
}

struct destroying_pool {
	char *name;
	uint64_t guid;
	char *endpoint;
	char *bucket;
	uint64_t start_time;
	uint64_t total_data_objects;
	uint64_t destroyed_objects;
	boolean_t destroyed;
};

/*
 * Callback function for printing the status of a pool that is being destroyed.
 */
static boolean_t
print_destroying_item(struct destroying_pool item)
{
	time_t tsec;
	char tbuf[64] = "";
	struct tm t;

	if (item.start_time != 0) {
		tsec = (time_t)item.start_time;
		(void) localtime_r(&tsec, &t);
		(void) strftime(tbuf, sizeof (tbuf), "%F.%T", &t);
	}
	uint64_t pct = item.total_data_objects == 0 ? 0 :
	    ((double)item.destroyed_objects / item.total_data_objects) * 100;
	char *state = item.destroyed ? "DESTROYED" : "DESTROYING";

	// Print pool status.
	printf("\n  pool: %s", item.name);
	printf("\n  guid: %lu\n", item.guid);
	printf(" state: %s\n", state);
	if (item.destroyed) {
		printf("status: The pool has been destroyed.\n");
		if (item.start_time != 0) {
			printf("        zpool destroy was initiated at %s UTC "
			    "and is complete.\n", tbuf);
		}
	} else {
		printf("status: The pool is being destroyed.\n");
		if (item.start_time != 0) {
			printf("        zpool destroy was initiated at %s UTC "
			    "and is %lu%% complete.\n", tbuf, pct);
		}
	}
	printf("config:\n\n");
	printf("        NAME                 STATE\n");
	printf("        %-20s %s\n", item.name,  state);
	printf("          %s:%s %s\n", item.endpoint, item.bucket, state);

	return (B_TRUE);
}

static void
zoa_list_destroy_pools(libpc_handle_t *hdl, boolean_t destroy_complete)
{
	nvlist_t *msg = fnvlist_alloc();
	fnvlist_add_string(msg, AGENT_REQUEST_TYPE,
	    AGENT_TYPE_GET_DESTROYING_POOLS);

	nvlist_t *resp = zoa_send_recv_msg(hdl, msg, AGENT_PROTOCOL_VERSION,
	    ZFS_PUBLIC_SOCKET);
	if (resp == NULL)
		return;

	const char *type = fnvlist_lookup_string(resp, AGENT_RESPONSE_TYPE);
	VERIFY0(strcmp(type, AGENT_TYPE_GET_DESTROYING_POOLS));

	nvlist_t *nvpools = NULL;
	(void) nvlist_lookup_nvlist(resp, AGENT_POOLS, &nvpools);


	nvpair_t *elem = NULL;
	struct destroying_pool item = { 0 };
	while ((elem = nvlist_next_nvpair(nvpools, elem)) != NULL) {
		nvlist_t *config;
		VERIFY0(nvpair_value_nvlist(elem, &config));

		item.destroyed = fnvlist_lookup_boolean_value(config,
		    AGENT_DESTROY_DOMPLETED);
		if (item.destroyed != destroy_complete) {
			continue;
		}

		item.name = fnvlist_lookup_string(config, AGENT_NAME);
		item.guid =
		    fnvlist_lookup_uint64(config, AGENT_GUID);
		item.bucket = fnvlist_lookup_string(config, AGENT_BUCKET);
		item.endpoint = fnvlist_lookup_string(config, AGENT_ENDPOINT);
		// Optional componenents
		(void) nvlist_lookup_uint64(config, AGENT_START_TIME,
		    &item.start_time);
		(void) nvlist_lookup_uint64(config, AGENT_TOTAL_DATA_OBJECTS,
		    &item.total_data_objects);
		(void) nvlist_lookup_uint64(config, AGENT_DESTROYED_OBJECTS,
		    &item.destroyed_objects);

		print_destroying_item(item);
	}

	fnvlist_free(resp);
}

/*
 * Print a status message for pools that have been completely destroyed are
 * also included.
 */

void
zoa_list_destroyed_pools(void *hdl)
{
	libpc_handle_t handle = { 0 };

	handle.lpc_lib_handle = hdl;
	handle.lpc_printerr = B_TRUE;

	zoa_list_destroy_pools(&handle, B_TRUE);
}

/*
 * Print a status message for pools that are being destroyed.
 */
void
zoa_list_destroying_pools(void *hdl)
{
	libpc_handle_t handle = { 0 };

	handle.lpc_lib_handle = hdl;
	handle.lpc_printerr = B_TRUE;

	zoa_list_destroy_pools(&handle, B_FALSE);
}

/*
 * Clear the destroyed pools so that they are not listed going forward.
 */
void
zoa_clear_destroyed_pools(void *hdl)
{
	libpc_handle_t handle = { 0 };

	handle.lpc_lib_handle = hdl;
	handle.lpc_printerr = B_TRUE;

	nvlist_t *msg = fnvlist_alloc();
	fnvlist_add_string(msg, AGENT_REQUEST_TYPE,
	    AGENT_TYPE_CLEAR_DESTROYED_POOLS);

	zoa_send_recv_msg(&handle, msg, AGENT_PROTOCOL_VERSION,
	    ZFS_PUBLIC_SOCKET);
}

nvlist_t *
zoa_create_connection_nvl(const char *endpoint, const char *region,
    const char *bucket, const char *creds_profile)
{
	nvlist_t *nvl = fnvlist_alloc();
	fnvlist_add_string(nvl, AGENT_ENDPOINT, endpoint);
	fnvlist_add_string(nvl, AGENT_REGION, region);
	fnvlist_add_string(nvl, AGENT_BUCKET, bucket);
	fnvlist_add_string(nvl, AGENT_CRED_PROFILE, creds_profile);

	return (nvl);
}
