#define _GNU_SOURCE
/*
 * uur-pw-capture — PipeWire ScreenCast consumer.
 *
 * Receives an inherited portal PipeWire fd (UUR_PW_FD) and authorized monitor
 * stream specifications (UUR_PW_STREAMS), composites their BGRx frames and
 * publishes the virtual desktop into UUR_FRAME_PATH using the seqlock protocol
 * in docs/capture-protocol.md.
 *
 * The shared file is created only after the stream format is negotiated,
 * because its size depends on the negotiated stride.  The uur PE hook maps
 * it read-only through Wine's Z: drive whenever the client captures.
 */

#include <locale.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <linux/dma-buf.h>
#include <poll.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

#include <pipewire/pipewire.h>
#include <spa/param/video/format-utils.h>
#include <drm/drm_fourcc.h>
#include "../hook/frame-protocol.h"

#define FRAME_FORMAT_BGRX 1
#ifndef SPA_CHUNK_FLAG_EMPTY
#define SPA_CHUNK_FLAG_EMPTY (1u << 1)
#endif

struct frame_header {
    uint32_t magic;
    uint32_t version;
    uint32_t width;
    uint32_t height;
    uint32_t stride;
    uint32_t format;
    _Atomic uint64_t total_frames;
    uint32_t flags;
    uint32_t monitor_count;
    uint8_t reserved[24];
    struct uurf_monitor_layout monitors[UURF_MAX_MONITORS];
};

_Static_assert(sizeof(struct frame_header) == UURF_HEADER_BYTES,
               "frame protocol header size changed");

static uint32_t scale_floor(uint64_t value, uint32_t numerator,
                            uint32_t denominator);
static uint64_t scale_ceil(uint64_t value, uint32_t numerator,
                           uint32_t denominator);

struct frame_slot {
    _Atomic uint64_t seq;
    /* frame bytes follow */
};

struct capture;

struct capture_stream {
    struct capture *capture;
    struct pw_stream *stream;
    struct spa_hook listener;
    uint32_t node_id;
    int32_t x;
    int32_t y;
    int32_t layout_x;
    int32_t layout_y;
    uint32_t logical_width;
    uint32_t logical_height;
    uint32_t width;
    uint32_t height;
    enum spa_video_format format;
    uint8_t *pixels;
    size_t pixels_size;
    int dmabuf_usable;
    int format_ready;
    int has_frame;
    int was_streaming;
    uint64_t paused_ms;
    uint64_t counter;
};

struct capture {
    struct pw_main_loop *loop;
    struct capture_stream streams[16];
    uint32_t stream_count;
    uint32_t width;
    uint32_t height;
    uint32_t stride;
    uint32_t flags;
    uint64_t counter;
    uint8_t *composite;
    size_t composite_size;
    int32_t layout_x;
    int32_t layout_y;
    uint32_t layout_width;
    uint32_t layout_height;
    uint32_t scale_num;
    uint32_t scale_den;
    int shm_fd;
    uint8_t *shm;
    size_t shm_size;
    int exit_status;
    volatile sig_atomic_t stopping;
    uint64_t started_ms;
    char frame_path[PATH_MAX];
};

static struct capture *active_capture;

static uint64_t monotonic_ms(void)
{
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return (uint64_t)now.tv_sec * 1000 + (uint64_t)now.tv_nsec / 1000000;
}

static void stop_capture(int signal_number)
{
    (void)signal_number;
    if (active_capture != NULL && active_capture->loop != NULL) {
        active_capture->stopping = 1;
        pw_main_loop_quit(active_capture->loop);
    }
}

static struct frame_slot *slot_at(struct capture *c, uint64_t n)
{
    size_t slot_bytes = sizeof(struct frame_slot) +
                        (size_t)c->stride * c->height;
    uint8_t *base = c->shm + uurf_slot_offset(n, slot_bytes);
    return (struct frame_slot *)base;
}

static uint8_t *slot_frame(struct frame_slot *slot)
{
    return (uint8_t *)slot + sizeof(struct frame_slot);
}

/* Publish a new inode for every geometry. A Wine reader may still map the
 * previous inode: never truncate or resize memory another process is reading. */
static int shm_setup(struct capture *c)
{
    size_t slot_bytes = sizeof(struct frame_slot) +
                        (size_t)c->stride * c->height;
    size_t total = UURF_HEADER_BYTES + UURF_SLOT_COUNT * slot_bytes;

    if (c->stride == 0 || c->height == 0 ||
        (size_t)c->stride >
            (SIZE_MAX - UURF_HEADER_BYTES) / c->height / UURF_SLOT_COUNT) {
        fprintf(stderr, "invalid capture geometry\n");
        return -1;
    }

    char temporary[PATH_MAX];
    if (snprintf(temporary, sizeof(temporary), "%s.XXXXXX", c->frame_path) >=
        (int)sizeof(temporary))
        return -1;
    int fd = mkostemp(temporary, O_CLOEXEC);
    if (fd < 0) {
        perror("open frame transport");
        return -1;
    }
    if (fchmod(fd, 0600) < 0 || ftruncate(fd, (off_t)total) < 0) {
        perror("ftruncate");
        close(fd);
        unlink(temporary);
        return -1;
    }
    uint8_t *mapping = mmap(NULL, total, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (mapping == MAP_FAILED) {
        perror("mmap");
        close(fd);
        unlink(temporary);
        return -1;
    }

    struct frame_header *header = (struct frame_header *)mapping;
    header->magic = 0x46525555u; /* "UURF" */
    header->version = UURF_VERSION;
    header->width = c->width;
    header->height = c->height;
    header->stride = c->stride;
    header->format = FRAME_FORMAT_BGRX;
    atomic_init(&header->total_frames, 0);
    header->flags = c->flags;
    header->monitor_count = c->stream_count;
    for (uint32_t i = 0; i < c->stream_count; ++i) {
        const struct capture_stream *s = &c->streams[i];
        struct uurf_monitor_layout *monitor = &header->monitors[i];
        monitor->logical_x = s->layout_x;
        monitor->logical_y = s->layout_y;
        monitor->logical_width = s->logical_width;
        monitor->logical_height = s->logical_height;
        if (c->flags == 0) {
            monitor->frame_x = 0;
            monitor->frame_y = 0;
            monitor->frame_width = c->width;
            monitor->frame_height = c->height;
        } else {
            uint64_t left = (uint64_t)((int64_t)s->layout_x - c->layout_x);
            uint64_t top = (uint64_t)((int64_t)s->layout_y - c->layout_y);
            uint32_t x0 = scale_floor(left, c->scale_num, c->scale_den);
            uint32_t y0 = scale_floor(top, c->scale_num, c->scale_den);
            uint32_t x1 = scale_ceil(left + s->logical_width,
                                     c->scale_num, c->scale_den);
            uint32_t y1 = scale_ceil(top + s->logical_height,
                                     c->scale_num, c->scale_den);
            monitor->frame_x = x0;
            monitor->frame_y = y0;
            monitor->frame_width = x1 > x0 ? x1 - x0 : 1;
            monitor->frame_height = y1 > y0 ? y1 - y0 : 1;
        }
    }
    for (uint32_t i = 0; i < UURF_SLOT_COUNT; ++i) {
        struct frame_slot *slot = (struct frame_slot *)(mapping +
            UURF_HEADER_BYTES + i * slot_bytes);
        atomic_init(&slot->seq, 0);
    }
    if (rename(temporary, c->frame_path) < 0) {
        perror("publish frame transport");
        munmap(mapping, total);
        close(fd);
        unlink(temporary);
        return -1;
    }
    if (c->shm != NULL) {
        struct frame_header *previous = (struct frame_header *)c->shm;
        atomic_store_explicit(&previous->total_frames, 0, memory_order_release);
        munmap(c->shm, c->shm_size);
    }
    if (c->shm_fd >= 0)
        close(c->shm_fd);
    c->shm = mapping;
    c->shm_fd = fd;
    c->shm_size = total;
    c->counter = 0;
    c->started_ms = monotonic_ms();
    return 0;
}

static void publish_frame(struct capture *c)
{
    if (c->shm == NULL || c->composite == NULL)
        return;

    c->counter += 1;
    struct frame_slot *slot = slot_at(c, c->counter);
    atomic_store_explicit(&slot->seq, 2 * c->counter - 1,
                          memory_order_release); /* writing */

    uint8_t *dst = slot_frame(slot);
    memcpy(dst, c->composite, c->composite_size);

    atomic_store_explicit(&slot->seq, 2 * c->counter, memory_order_release);
    struct frame_header *header = (struct frame_header *)c->shm;
    atomic_store_explicit(&header->total_frames, c->counter,
                          memory_order_release);
}

static int sync_dmabuf(int fd, uint64_t flags)
{
    struct dma_buf_sync sync = {.flags = flags};
    int result;
    do {
        result = ioctl(fd, DMA_BUF_IOCTL_SYNC, &sync);
    } while (result < 0 && errno == EINTR);
    return result;
}

static int all_streams_ready(const struct capture *c)
{
    for (uint32_t i = 0; i < c->stream_count; ++i) {
        if (!c->streams[i].format_ready)
            return 0;
    }
    return c->stream_count != 0;
}

static int all_streams_have_frames(const struct capture *c)
{
    for (uint32_t i = 0; i < c->stream_count; ++i) {
        if (!c->streams[i].has_frame)
            return 0;
    }
    return c->stream_count != 0;
}

static int ratio_less(uint64_t left_num, uint64_t left_den,
                      uint64_t right_num, uint64_t right_den)
{
    return left_num * right_den < right_num * left_den;
}

static uint32_t scale_floor(uint64_t value, uint32_t numerator,
                            uint32_t denominator)
{
    return denominator == 0 ? 0 : (uint32_t)(value * numerator / denominator);
}

static uint64_t scale_ceil(uint64_t value, uint32_t numerator,
                           uint32_t denominator)
{
    return denominator == 0 ? 0 :
        (value * numerator + denominator - 1) / denominator;
}

static int configure_layout(struct capture *c)
{
    uint32_t width, height, flags;
    uint64_t width64, height64;
    int missing_position = 0;
    int64_t min_x = INT64_MAX, min_y = INT64_MAX;
    int64_t max_x = INT64_MIN, max_y = INT64_MIN;
    uint32_t scale_num = 0, scale_den = 1;

    if (!all_streams_ready(c))
        return 0;
    for (uint32_t i = 0; i < c->stream_count; ++i) {
        struct capture_stream *s = &c->streams[i];
        if (s->logical_width == 0)
            s->logical_width = s->width;
        if (s->logical_height == 0)
            s->logical_height = s->height;
        if (s->x == INT32_MIN || s->y == INT32_MIN)
            missing_position = 1;
    }

    if (c->stream_count == 1) {
        struct capture_stream *s = &c->streams[0];
        width64 = s->width;
        height64 = s->height;
        flags = 0;
        s->layout_x = 0;
        s->layout_y = 0;
        c->layout_x = 0;
        c->layout_y = 0;
        c->layout_width = s->width;
        c->layout_height = s->height;
        scale_num = 1;
        scale_den = 1;
    } else {
        int64_t next_x = 0;
        for (uint32_t i = 0; i < c->stream_count; ++i) {
            struct capture_stream *s = &c->streams[i];
            s->layout_x = missing_position ? (int32_t)next_x : s->x;
            s->layout_y = missing_position ? 0 : s->y;
            next_x = (int64_t)s->layout_x + s->logical_width;
            int64_t right = (int64_t)s->layout_x + s->logical_width;
            int64_t bottom = (int64_t)s->layout_y + s->logical_height;
            if (s->layout_x < min_x)
                min_x = s->layout_x;
            if (s->layout_y < min_y)
                min_y = s->layout_y;
            if (right > max_x)
                max_x = right;
            if (bottom > max_y)
                max_y = bottom;

            uint64_t ratio_num = s->width;
            uint64_t ratio_den = s->logical_width;
            if (ratio_less(s->height, s->logical_height, ratio_num, ratio_den)) {
                ratio_num = s->height;
                ratio_den = s->logical_height;
            }
            if (scale_num == 0 || ratio_less(ratio_num, ratio_den,
                                             scale_num, scale_den)) {
                scale_num = (uint32_t)ratio_num;
                scale_den = (uint32_t)ratio_den;
            }
        }
        if (min_x == INT64_MAX || min_y == INT64_MAX || max_x <= min_x ||
            max_y <= min_y || scale_num == 0 || scale_den == 0 ||
            min_x < INT32_MIN || min_x > INT32_MAX || min_y < INT32_MIN ||
            min_y > INT32_MAX || max_x - min_x > UINT32_MAX ||
            max_y - min_y > UINT32_MAX) {
            fprintf(stderr, "invalid monitor stream layout\n");
            return -1;
        }
        c->layout_x = (int32_t)min_x;
        c->layout_y = (int32_t)min_y;
        c->layout_width = (uint32_t)(max_x - min_x);
        c->layout_height = (uint32_t)(max_y - min_y);
        width64 = scale_ceil(c->layout_width, scale_num, scale_den);
        height64 = scale_ceil(c->layout_height, scale_num, scale_den);
        flags = UURF_FLAG_COMPOSITE;
    }

    if (width64 == 0 || height64 == 0 || width64 > 16384 || height64 > 16384 ||
        width64 * 4 * height64 > 256u * 1024 * 1024) {
        fprintf(stderr, "composited capture geometry is unsupported\n");
        return -1;
    }
    width = (uint32_t)width64;
    height = (uint32_t)height64;

    int changed = c->width != width || c->height != height || c->flags != flags;
    c->width = width;
    c->height = height;
    c->stride = width * 4;
    c->flags = flags;
    c->scale_num = scale_num;
    c->scale_den = scale_den;
    if (changed || c->composite == NULL || c->shm == NULL) {
        size_t bytes = (size_t)c->stride * height;
        uint8_t *fresh = realloc(c->composite, bytes);
        if (fresh == NULL) {
            fprintf(stderr, "could not allocate composited capture frame\n");
            return -1;
        }
        c->composite = fresh;
        c->composite_size = bytes;
        memset(c->composite, 0, bytes);
        if (shm_setup(c) < 0)
            return -1;
    }
    return 1;
}

static void compose_frame(struct capture *c)
{
    if (c->composite == NULL || c->width == 0 || c->height == 0)
        return;
    if (c->stream_count == 1) {
        memcpy(c->composite, c->streams[0].pixels, c->composite_size);
        return;
    }

    memset(c->composite, 0, c->composite_size);
    for (uint32_t i = 0; i < c->stream_count; ++i) {
        struct capture_stream *s = &c->streams[i];
        uint64_t left_logical = (uint64_t)((int64_t)s->layout_x - c->layout_x);
        uint64_t top_logical = (uint64_t)((int64_t)s->layout_y - c->layout_y);
        uint32_t left = scale_floor(left_logical, c->scale_num, c->scale_den);
        uint32_t top = scale_floor(top_logical, c->scale_num, c->scale_den);
        uint32_t right = scale_ceil(left_logical + s->logical_width,
                                    c->scale_num, c->scale_den);
        uint32_t bottom = scale_ceil(top_logical + s->logical_height,
                                     c->scale_num, c->scale_den);
        uint32_t target_width = right > left ? right - left : 1;
        uint32_t target_height = bottom > top ? bottom - top : 1;
        if (left >= c->width || top >= c->height)
            continue;
        if (target_width > c->width - left)
            target_width = c->width - left;
        if (target_height > c->height - top)
            target_height = c->height - top;
        for (uint32_t row = 0; row < target_height; ++row) {
            uint32_t source_y = (uint32_t)((uint64_t)row * s->height / target_height);
            uint8_t *target = c->composite + (size_t)(top + row) * c->stride + (size_t)left * 4;
            const uint8_t *source = s->pixels + (size_t)source_y * s->width * 4;
            for (uint32_t column = 0; column < target_width; ++column) {
                uint32_t source_x = (uint32_t)((uint64_t)column * s->width / target_width);
                memcpy(target + (size_t)column * 4, source + (size_t)source_x * 4, 4);
            }
        }
    }
}

static void normalize_pixels(struct capture_stream *s, const uint8_t *pixels,
                             uint32_t source_stride)
{
    uint32_t source_bpp =
        s->format == SPA_VIDEO_FORMAT_BGR || s->format == SPA_VIDEO_FORMAT_RGB ? 3u : 4u;
    if (s->format == SPA_VIDEO_FORMAT_BGRx || s->format == SPA_VIDEO_FORMAT_BGRA) {
        for (uint32_t row = 0; row < s->height; ++row) {
            memcpy(s->pixels + (size_t)row * s->width * 4,
                   pixels + (size_t)row * source_stride, (size_t)s->width * 4);
        }
        return;
    }
    for (uint32_t row = 0; row < s->height; ++row) {
        const uint8_t *source = pixels + (size_t)row * source_stride;
        uint8_t *target = s->pixels + (size_t)row * s->width * 4;
        for (uint32_t column = 0; column < s->width; ++column) {
            int rgb = s->format == SPA_VIDEO_FORMAT_RGBx ||
                      s->format == SPA_VIDEO_FORMAT_RGBA ||
                      s->format == SPA_VIDEO_FORMAT_RGB;
            target[column * 4] = source[column * source_bpp + (rgb ? 2 : 0)];
            target[column * 4 + 1] = source[column * source_bpp + 1];
            target[column * 4 + 2] = source[column * source_bpp + (rgb ? 0 : 2)];
            target[column * 4 + 3] = 0;
        }
    }
}

static void on_process(void *userdata)
{
    struct capture_stream *s = userdata;
    struct capture *c = s->capture;
    struct pw_buffer *buffer = pw_stream_dequeue_buffer(s->stream);
    if (buffer == NULL)
        return;
    struct spa_buffer *buf = buffer->buffer;

    if (buf->n_datas >= 1 && s->format_ready && s->pixels != NULL) {
        struct spa_data *plane = &buf->datas[0];
        uint8_t *pixels = NULL;
        void *mapping = MAP_FAILED;
        size_t mapping_size = 0;
        int synchronized = 0;
        int dmabuf = plane->type == SPA_DATA_DmaBuf;

        if (plane->chunk == NULL || plane->chunk->stride < 0 ||
            (plane->chunk->flags & (SPA_CHUNK_FLAG_CORRUPTED | SPA_CHUNK_FLAG_EMPTY)))
            goto done;
        uint32_t source_bpp =
            s->format == SPA_VIDEO_FORMAT_BGR || s->format == SPA_VIDEO_FORMAT_RGB ? 3u : 4u;
        uint32_t row_bytes = s->width * source_bpp;
        uint32_t stride = plane->chunk->stride > 0 ?
            (uint32_t)plane->chunk->stride : row_bytes;
        uint64_t span = (uint64_t)stride * (s->height - 1) + row_bytes;
        uint32_t data_offset = plane->chunk->offset;
        if (stride < row_bytes || span > 256u * 1024 * 1024)
            goto done;
        if (dmabuf) {
            if (!s->dmabuf_usable || plane->fd < 0)
                goto done;
            off_t extent = lseek((int)plane->fd, 0, SEEK_END);
            uint64_t end = (uint64_t)plane->mapoffset + data_offset + span;
            if (extent < 0 || end > (uint64_t)extent)
                goto done;
            struct pollfd ready = {.fd = (int)plane->fd, .events = POLLIN};
            if (poll(&ready, 1, 100) <= 0 || !(ready.revents & POLLIN) ||
                sync_dmabuf((int)plane->fd, DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ) < 0)
                goto done;
            synchronized = 1;
        } else {
            if (plane->maxsize == 0)
                goto done;
            data_offset %= plane->maxsize;
            if ((uint64_t)data_offset + span > plane->maxsize ||
                span > plane->chunk->size)
                goto done;
        }
        if (plane->data != NULL) {
            pixels = (uint8_t *)plane->data + data_offset;
        } else if ((dmabuf || plane->type == SPA_DATA_MemFd) && plane->fd >= 0) {
            long page_size = sysconf(_SC_PAGESIZE);
            uint64_t offset = (uint64_t)plane->mapoffset + data_offset;
            uint64_t page_mask = (uint64_t)(page_size > 0 ? page_size : 4096) - 1;
            off_t aligned = (off_t)(offset & ~page_mask);
            size_t delta = (size_t)(offset - (uint64_t)aligned);
            mapping_size = delta + (size_t)span;
            mapping = mmap(NULL, mapping_size, PROT_READ, MAP_SHARED,
                           plane->fd, aligned);
            if (mapping != MAP_FAILED)
                pixels = (uint8_t *)mapping + delta;
        }
        if (pixels != NULL) {
            if (s->counter == 0)
                fprintf(stderr, "stream %u buffer: %s, source stride %u, chunk offset %u\n",
                    s->node_id, dmabuf ? "linear DMA-BUF" : "shared memory", stride, data_offset);
            normalize_pixels(s, pixels, stride);
            s->counter++;
            s->has_frame = 1;
            if (c->shm != NULL && all_streams_have_frames(c)) {
                compose_frame(c);
                publish_frame(c);
            }
        }
done:
        if (synchronized)
            (void)sync_dmabuf((int)plane->fd, DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ);
        if (mapping != MAP_FAILED)
            munmap(mapping, mapping_size);
    }

    pw_stream_queue_buffer(s->stream, buffer);
}

static void on_param_changed(void *userdata, uint32_t id,
                             const struct spa_pod *param)
{
    struct capture_stream *s = userdata;
    struct capture *c = s->capture;
    if (param == NULL || id != SPA_PARAM_Format)
        return;

    struct spa_video_info_raw info = {0};
    if (spa_format_video_raw_parse(param, &info) < 0)
        return;
    if (info.format != SPA_VIDEO_FORMAT_BGRx &&
        info.format != SPA_VIDEO_FORMAT_BGRA &&
        info.format != SPA_VIDEO_FORMAT_RGBx &&
        info.format != SPA_VIDEO_FORMAT_RGBA &&
        info.format != SPA_VIDEO_FORMAT_BGR &&
        info.format != SPA_VIDEO_FORMAT_RGB) {
        fprintf(stderr, "stream %u negotiated unexpected format %d\n",
                s->node_id, info.format);
        return;
    }

    if (info.size.width == 0 || info.size.height == 0 ||
        info.size.width > 16384 || info.size.height > 16384 ||
        (uint64_t)info.size.width * 4 * info.size.height > 256u * 1024 * 1024) {
        fprintf(stderr, "stream %u has unsupported capture geometry\n", s->node_id);
        c->exit_status = 1;
        pw_main_loop_quit(c->loop);
        return;
    }
    int changed = !s->format_ready || s->width != info.size.width ||
                  s->height != info.size.height || s->format != info.format;
    s->width = (uint32_t)info.size.width;
    s->height = (uint32_t)info.size.height;
    s->format = info.format;
    /* The DMA-BUF path mmaps the fd directly, which is only correct for a
     * linear layout.  A tiled modifier needs a real importer (GBM/EGL), so
     * refuse to publish garbage rather than emit a corrupt frame. */
    s->dmabuf_usable = spa_pod_find_prop(param, NULL, SPA_FORMAT_VIDEO_modifier) != NULL &&
                       info.modifier == DRM_FORMAT_MOD_LINEAR;
    if (changed) {
        size_t bytes = (size_t)s->width * s->height * 4;
        size_t source_total = bytes;
        for (uint32_t i = 0; i < c->stream_count; ++i) {
            if (&c->streams[i] != s)
                source_total += c->streams[i].pixels_size;
        }
        if (source_total > 512u * 1024 * 1024) {
            fprintf(stderr, "monitor source buffers exceed the memory limit\n");
            c->exit_status = 1;
            pw_main_loop_quit(c->loop);
            return;
        }
        uint8_t *fresh = realloc(s->pixels, bytes);
        if (fresh == NULL) {
            fprintf(stderr, "could not allocate stream %u frame buffer\n", s->node_id);
            c->exit_status = 1;
            pw_main_loop_quit(c->loop);
            return;
        }
        s->pixels = fresh;
        s->pixels_size = bytes;
        memset(s->pixels, 0, bytes);
        s->has_frame = 0;
    }
    s->format_ready = 1;
    fprintf(stderr, "stream %u negotiated: %ux%u modifier %#llx%s\n", s->node_id,
            s->width, s->height, (unsigned long long)info.modifier,
            s->dmabuf_usable ? "" : " (DMA-BUF CPU mapping disabled)");
    int layout = configure_layout(c);
    if (layout < 0) {
        c->exit_status = 1;
        pw_main_loop_quit(c->loop);
    } else if (layout > 0 && all_streams_have_frames(c)) {
        compose_frame(c);
        publish_frame(c);
    }
}

static void on_state_changed(void *userdata, enum pw_stream_state old,
                             enum pw_stream_state state, const char *error)
{
    struct capture_stream *s = userdata;
    struct capture *c = s->capture;
    fprintf(stderr, "stream %u state: %s%s%s\n", s->node_id,
            pw_stream_state_as_string(state),
            error != NULL && error[0] != '\0' ? " - " : "",
            error != NULL ? error : "");
    if (c->stopping)
        return;
    if (state == PW_STREAM_STATE_STREAMING) {
        s->was_streaming = 1;
        s->paused_ms = 0;
    } else if (state == PW_STREAM_STATE_PAUSED && s->was_streaming) {
        if (s->paused_ms == 0)
            s->paused_ms = monotonic_ms();
    } else if (state == PW_STREAM_STATE_ERROR ||
               (state == PW_STREAM_STATE_UNCONNECTED && old != state)) {
        c->exit_status = 75;
        pw_main_loop_quit(c->loop);
    }
}

static void on_watchdog(void *userdata, uint64_t expirations)
{
    struct capture *c = userdata;
    (void)expirations;
    uint64_t now = monotonic_ms();
    for (uint32_t i = 0; i < c->stream_count; ++i) {
        struct capture_stream *s = &c->streams[i];
        if (!c->stopping &&
            ((s->paused_ms != 0 && now - s->paused_ms >= 10000) ||
             (!s->has_frame && now - c->started_ms >= 30000))) {
            fprintf(stderr, "stream %u stopped producing usable frames; reconnecting\n",
                    s->node_id);
            c->exit_status = 75;
            pw_main_loop_quit(c->loop);
            return;
        }
    }
}

static void on_core_error(void *userdata, uint32_t id, int sequence,
                           int result, const char *message)
{
    struct capture *c = userdata;
    (void)sequence;
    if (!c->stopping && id == PW_ID_CORE && result < 0) {
        fprintf(stderr, "PipeWire connection failed: %s\n", message);
        c->exit_status = 75;
        pw_main_loop_quit(c->loop);
    }
}

static const struct pw_core_events core_events = {
    PW_VERSION_CORE_EVENTS,
    .error = on_core_error,
};

static const struct pw_stream_events stream_events = {
    .version = PW_VERSION_STREAM_EVENTS,
    .process = on_process,
    .param_changed = on_param_changed,
    .state_changed = on_state_changed,
};

static const struct spa_pod *capture_format(struct spa_pod_builder *builder,
                                           int dmabuf)
{
    struct spa_pod_frame frame;
    spa_pod_builder_push_object(builder, &frame, SPA_TYPE_OBJECT_Format,
                               SPA_PARAM_EnumFormat);
    spa_pod_builder_add(builder,
        SPA_FORMAT_mediaType, SPA_POD_Id(SPA_MEDIA_TYPE_video),
        SPA_FORMAT_mediaSubtype, SPA_POD_Id(SPA_MEDIA_SUBTYPE_raw),
        SPA_FORMAT_VIDEO_format,
        SPA_POD_CHOICE_ENUM_Id(6, SPA_VIDEO_FORMAT_BGRx, SPA_VIDEO_FORMAT_BGRA,
            SPA_VIDEO_FORMAT_RGBx, SPA_VIDEO_FORMAT_RGBA,
            SPA_VIDEO_FORMAT_BGR, SPA_VIDEO_FORMAT_RGB), 0);
    if (dmabuf) {
        spa_pod_builder_prop(builder, SPA_FORMAT_VIDEO_modifier,
                             SPA_POD_PROP_FLAG_MANDATORY);
        spa_pod_builder_long(builder, DRM_FORMAT_MOD_LINEAR);
    }
    return spa_pod_builder_pop(builder, &frame);
}

static int parse_stream_specs(struct capture *c, const char *text)
{
    char *copy = strdup(text);
    if (copy == NULL)
        return -1;
    char *save = NULL;
    char *item = strtok_r(copy, ";", &save);
    while (item != NULL) {
        unsigned node, width, height;
        int x, y;
        if (c->stream_count >= sizeof(c->streams) / sizeof(c->streams[0]) ||
            sscanf(item, "%u,%d,%d,%u,%u", &node, &x, &y, &width, &height) != 5) {
            free(copy);
            return -1;
        }
        struct capture_stream *s = &c->streams[c->stream_count++];
        s->node_id = node;
        s->x = x;
        s->y = y;
        s->logical_width = width;
        s->logical_height = height;
        item = strtok_r(NULL, ";", &save);
    }
    free(copy);
    return c->stream_count == 0 ? -1 : 0;
}

int main(void)
{
    const char *fd_text = getenv("UUR_PW_FD");
    const char *spec_text = getenv("UUR_PW_STREAMS");
    const char *legacy_node = getenv("UUR_PW_NODE");
    const char *frame_path = getenv("UUR_FRAME_PATH");
    if (fd_text == NULL || frame_path == NULL || frame_path[0] != '/' ||
        strlen(frame_path) >= PATH_MAX) {
        fprintf(stderr, "UUR_PW_FD and an absolute UUR_FRAME_PATH are required\n");
        return 2;
    }
    int pw_fd = atoi(fd_text);

    struct capture c;
    memset(&c, 0, sizeof(c));
    c.shm_fd = -1;
    if (spec_text != NULL) {
        if (parse_stream_specs(&c, spec_text) < 0) {
            fprintf(stderr, "invalid UUR_PW_STREAMS specification\n");
            return 2;
        }
    } else if (legacy_node != NULL) {
        char *end = NULL;
        errno = 0;
        unsigned long node = strtoul(legacy_node, &end, 10);
        if (errno != 0 || end == legacy_node || *end != '\0' || node > UINT32_MAX) {
            fprintf(stderr, "invalid UUR_PW_NODE\n");
            return 2;
        }
        c.stream_count = 1;
        c.streams[0].node_id = (uint32_t)node;
        c.streams[0].x = INT32_MIN;
        c.streams[0].y = INT32_MIN;
    } else {
        fprintf(stderr, "UUR_PW_STREAMS must be set by uur\n");
        return 2;
    }
    memcpy(c.frame_path, frame_path, strlen(frame_path) + 1);
    fprintf(stderr, "capturing %u ScreenCast stream(s)\n", c.stream_count);

    pw_init(NULL, NULL);
    active_capture = &c;
    signal(SIGINT, stop_capture);
    signal(SIGTERM, stop_capture);
    signal(SIGHUP, stop_capture);

    struct pw_main_loop *loop = pw_main_loop_new(NULL);
    if (loop == NULL) {
        fprintf(stderr, "pw_main_loop_new failed\n");
        return 1;
    }
    c.loop = loop;

    struct pw_context *context = pw_context_new(pw_main_loop_get_loop(loop),
                                                 NULL, 0);
    if (context == NULL) {
        fprintf(stderr, "pw_context_new failed\n");
        return 1;
    }

    /* The portal fd only exposes streams covered by this ScreenCast consent. */
    struct pw_core *core = pw_context_connect_fd(context, pw_fd, NULL, 0);
    if (core == NULL) {
        fprintf(stderr, "pw_context_connect_fd failed\n");
        return 1;
    }
    struct spa_hook core_listener;
    pw_core_add_listener(core, &core_listener, &core_events, &c);
    struct spa_source *watchdog = NULL;

    uint8_t pod_buffer[1024];
    struct spa_pod_builder pod_builder =
        SPA_POD_BUILDER_INIT(pod_buffer, sizeof(pod_buffer));
    const struct spa_pod *params[2];
    params[0] = capture_format(&pod_builder, 1);
    params[1] = capture_format(&pod_builder, 0);
    c.started_ms = monotonic_ms();

    for (uint32_t i = 0; i < c.stream_count; ++i) {
        struct capture_stream *s = &c.streams[i];
        s->capture = &c;
        char name[64];
        snprintf(name, sizeof(name), "uur-capture-%u", i + 1);
        struct pw_properties *props = pw_properties_new(
            PW_KEY_MEDIA_TYPE, "Video", PW_KEY_MEDIA_CATEGORY, "Capture",
            PW_KEY_MEDIA_ROLE, "Screen", NULL);
        s->stream = pw_stream_new(core, name, props);
        if (s->stream == NULL) {
            fprintf(stderr, "pw_stream_new failed for node %u\n", s->node_id);
            c.exit_status = 1;
            goto cleanup;
        }
        pw_stream_add_listener(s->stream, &s->listener, &stream_events, s);
        if (pw_stream_connect(s->stream, PW_DIRECTION_INPUT, s->node_id,
                              PW_STREAM_FLAG_MAP_BUFFERS |
                                  PW_STREAM_FLAG_AUTOCONNECT,
                              params, 2) < 0) {
            fprintf(stderr, "pw_stream_connect failed for node %u\n", s->node_id);
            c.exit_status = 1;
            goto cleanup;
        }
    }

    watchdog = pw_loop_add_timer(pw_main_loop_get_loop(loop), on_watchdog, &c);
    struct timespec interval = {.tv_sec = 1};
    if (watchdog == NULL || pw_loop_update_timer(pw_main_loop_get_loop(loop),
            watchdog, &interval, &interval, false) < 0) {
        fprintf(stderr, "could not start capture watchdog\n");
        c.exit_status = 1;
        goto cleanup;
    }
    pw_main_loop_run(loop);

    c.stopping = 1;
    active_capture = NULL;
cleanup:
    c.stopping = 1;
    active_capture = NULL;
    if (watchdog != NULL)
        pw_loop_destroy_source(pw_main_loop_get_loop(loop), watchdog);
    for (uint32_t i = 0; i < c.stream_count; ++i) {
        struct capture_stream *s = &c.streams[i];
        if (s->stream != NULL) {
            pw_stream_destroy(s->stream);
            spa_hook_remove(&s->listener);
        }
        free(s->pixels);
    }
    spa_hook_remove(&core_listener);
    pw_core_disconnect(core);
    pw_context_destroy(context);
    pw_main_loop_destroy(loop);
    if (c.shm != NULL) {
        struct frame_header *header = (struct frame_header *)c.shm;
        atomic_store_explicit(&header->total_frames, 0, memory_order_release);
        munmap(c.shm, c.shm_size);
    }
    if (c.shm_fd >= 0)
        close(c.shm_fd);
    free(c.composite);
    unlink(c.frame_path);
    pw_deinit();
    return c.exit_status;
}
