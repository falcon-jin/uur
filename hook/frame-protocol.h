#ifndef UUR_FRAME_PROTOCOL_H
#define UUR_FRAME_PROTOCOL_H

#include <stddef.h>
#include <stdint.h>

#define UURF_MAGIC 0x46525555u
#define UURF_VERSION 2u
#define UURF_SLOT_COUNT 3u
#define UURF_BASE_HEADER_BYTES 64u
#define UURF_MAX_MONITORS 16u
#define UURF_MONITOR_BYTES 32u
#define UURF_HEADER_BYTES \
    (UURF_BASE_HEADER_BYTES + UURF_MAX_MONITORS * UURF_MONITOR_BYTES)
#define UURF_FLAG_COMPOSITE 1u

struct uurf_monitor_layout {
    int32_t logical_x;
    int32_t logical_y;
    uint32_t logical_width;
    uint32_t logical_height;
    uint32_t frame_x;
    uint32_t frame_y;
    uint32_t frame_width;
    uint32_t frame_height;
};

static inline size_t uurf_slot_offset(uint64_t sequence, size_t slot_bytes)
{
    return UURF_HEADER_BYTES +
           (size_t)(sequence % UURF_SLOT_COUNT) * slot_bytes;
}

#endif
