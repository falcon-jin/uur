#ifndef UUR_FRAME_PROTOCOL_H
#define UUR_FRAME_PROTOCOL_H

#include <stddef.h>
#include <stdint.h>

#define UURF_MAGIC 0x46525555u
#define UURF_VERSION 1u
#define UURF_SLOT_COUNT 3u
#define UURF_HEADER_BYTES 64u
#define UURF_FLAG_COMPOSITE 1u

static inline size_t uurf_slot_offset(uint64_t sequence, size_t slot_bytes)
{
    return UURF_HEADER_BYTES +
           (size_t)(sequence % UURF_SLOT_COUNT) * slot_bytes;
}

#endif
