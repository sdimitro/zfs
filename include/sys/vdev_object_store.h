/*
 * CDDL HEADER START
 *
 * This file and its contents are supplied under the terms of the
 * Common Development and Distribution License ("CDDL"), version 1.0.
 * You may only use this file in accordance with the terms of version
 * 1.0 of the CDDL.
 *
 * A full copy of the text of the CDDL should have accompanied this
 * source.  A copy of the CDDL is also available via the Internet at
 * http://www.illumos.org/license/CDDL.
 *
 * CDDL HEADER END
 */
/*
 * Copyright (c) 2021, 2022 by Delphix. All rights reserved.
 */

#include <sys/zfs_context.h>

/*
 * Possible keys in nvlist requests / responses to/from the Agent
 */
#define	AGENT_REQUEST_TYPE		"request_type"
#define	AGENT_RESPONSE_TYPE		"response_type"

#define	AGENT_TYPE_CREATE_POOL		"create pool"
#define	AGENT_TYPE_OPEN_POOL		"open pool"
#define	AGENT_TYPE_FREE_BLOCKS		"free blocks"
#define	AGENT_TYPE_BEGIN_TXG		"begin txg"
#define	AGENT_TYPE_RESUME_COMPLETE	"resume complete"
#define	AGENT_TYPE_END_TXG		"end txg"
#define	AGENT_TYPE_FLUSH_WRITES		"flush writes"
#define	AGENT_TYPE_GET_STATS		"get stats"
#define	AGENT_TYPE_EXIT			"exit agent"
#define	AGENT_TYPE_CLOSE_POOL		"close pool"
#define	AGENT_TYPE_ENABLE_FEATURE	"enable feature"
#define	AGENT_TYPE_GET_POOLS		"get pools"
#define	AGENT_TYPE_GET_DESTROYING_POOLS	"get destroying pools"
#define	AGENT_TYPE_CLEAR_DESTROYED_POOLS "clear destroyed pools"
#define	AGENT_TYPE_RESUME_DESTROY_POOL	"resume destroy pool"
#define	AGENT_TYPE_VERSION		"get version"

#define	AGENT_ERR			"err"
#define	AGENT_MESSAGE			"message"
#define	AGENT_NAME			"name"
#define	AGENT_SIZE			"size"
#define	AGENT_TXG			"txg"
#define	AGENT_GUID			"guid"
#define	AGENT_BUCKET			"bucket"
#define	AGENT_CRED_PROFILE		"credentials_profile"
#define	AGENT_PROTOCOL			"protocol"
#define	AGENT_ENDPOINT			"endpoint"
#define	AGENT_REGION			"region"
#define	AGENT_BLOCK			"block"
#define	AGENT_DATA			"data"
#define	AGENT_STATS			"stats"
#define	AGENT_UBERBLOCK			"uberblock"
#define	AGENT_CONFIG			"config"
#define	AGENT_NEXT_BLOCK		"next_block"
#define	AGENT_TOKEN			"token"
#define	AGENT_READONLY			"readonly"
#define	AGENT_SYNCING_TXG		"syncing_txg"
#define	AGENT_FEATURE			"feature"
#define	AGENT_FEATURES			"features"
#define	AGENT_DESTROY			"destroy"
#define	AGENT_DESTROY_COMPLETED		"destroy_completed"
#define	AGENT_DESTROY_STATE		"state"
#define	AGENT_DESTROY_STATE_COMPLETE	"Complete"
#define	AGENT_START_TIME		"start_time"
#define	AGENT_TOTAL_DATA_OBJECTS	"total_data_objects"
#define	AGENT_DESTROYED_OBJECTS		"destroyed_objects"
#define	AGENT_POOLS			"pools"
#define	AGENT_CHECKPOINT		"checkpoint"
#define	AGENT_ROLLBACK			"rollback"
#define	AGENT_REISSUE			"reissue"
#define	AGENT_VERSION			"version"
#define	AGENT_VERSION_MAJOR		"major"
#define	AGENT_VERSION_MINOR		"minor"
#define	AGENT_VERSION_PATCH		"patch"

typedef struct vdev_object_store_stats {
	uint64_t voss_blocks_count;
	uint64_t voss_blocks_bytes;
	uint64_t voss_pending_frees_count;
	uint64_t voss_pending_frees_bytes;
	uint64_t voss_objects_count;
} vdev_object_store_stats_t;

/*
 * XXX this should be auto-generated from the rust type
 */
typedef enum MessageType {
	MESSAGE_NVLIST,
	MESSAGE_READ_BLOCK,
	MESSAGE_WRITE_BLOCK,
} message_type_t;

/*
 * XXX this should be auto-generated from the rust type
 */
typedef struct MessageHeader {
    uint32_t message_type;
    uint32_t struct_len;
    uint32_t payload_len;
} message_header_t;

/*
 * XXX this should be auto-generated from the rust type
 */
typedef struct ReadBlockRequest {
    uint64_t block;
    uint64_t token;
    uint32_t heal;
    uint32_t size;
} read_block_request_t;

/*
 * XXX this should be auto-generated from the rust type
 */
typedef struct WriteBlockRequest {
    uint64_t block;
    uint64_t token;
} write_block_request_t;

/*
 * XXX this should be auto-generated from the rust type
 */
typedef struct ReadBlockResponse {
    uint64_t block;
    uint64_t token;
} read_block_response_t;

/*
 * XXX this should be auto-generated from the rust type
 */
typedef struct WriteBlockResponse {
    uint64_t block;
    uint64_t token;
} write_block_response_t;

void object_store_begin_txg(vdev_t *, uint64_t);
void object_store_end_txg(vdev_t *, nvlist_t *, uint64_t);
void object_store_free_block(vdev_t *, uint64_t, uint64_t);
void object_store_flush_all_writes(zio_t *);
void object_store_update_max_blockid(zio_t *);
void object_store_flush_locked_writes(spa_t *);
void object_store_restart_agent(vdev_t *);
void object_store_get_stats(vdev_t *, vdev_object_store_stats_t *);
