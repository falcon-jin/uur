#define _GNU_SOURCE
/*
 * uur-pw-capture — PipeWire ScreenCast consumer.
 *
 * Receives an inherited portal PipeWire fd (UUR_PW_FD) and the authorized
 * video node id (UUR_PW_NODE), streams the monitor as BGRx and publishes
 * frames into the private UUR_FRAME_PATH following the seqlock protocol in
 * docs/capture-protocol.md.
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
    uint8_t reserved[32];
};

_Static_assert(sizeof(struct frame_header) == UURF_HEADER_BYTES,
               "frame protocol header size changed");

struct frame_slot {
    _Atomic uint64_t seq;
    /* frame bytes follow */
};

struct capture {
    struct pw_main_loop *loop;
    struct pw_stream *stream;
    struct spa_hook listener;
    uint32_t width;
    uint32_t height;
    uint32_t stride;
    enum spa_video_format format;
    uint64_t counter;
    int shm_fd;
    uint8_t *shm;
    size_t shm_size;
    int dmabuf_usable;
    int exit_status;
    volatile sig_atomic_t stopping;
    int was_streaming;
    uint64_t started_ms;
    uint64_t paused_ms;
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

static void publish_frame(struct capture *c, const uint8_t *pixels,
                          uint32_t src_stride, uint32_t src_height)
{
    if (c->shm == NULL)
        return;

    c->counter += 1;
    struct frame_slot *slot = slot_at(c, c->counter);
    atomic_store_explicit(&slot->seq, 2 * c->counter - 1,
                          memory_order_release); /* writing */

    uint8_t *dst = slot_frame(slot);
    uint32_t rows = src_height < c->height ? src_height : c->height;
    for (uint32_t row = 0; row < rows; ++row) {
        uint8_t *target = dst + (size_t)row * c->stride;
        const uint8_t *source = pixels + (size_t)row * src_stride;
        if (c->format == SPA_VIDEO_FORMAT_BGRx ||
            c->format == SPA_VIDEO_FORMAT_BGRA) {
            memcpy(target, source, c->stride);
        } else if (c->format == SPA_VIDEO_FORMAT_RGBx ||
                   c->format == SPA_VIDEO_FORMAT_RGBA) {
            for (uint32_t column = 0; column < c->width; ++column) {
                target[column * 4 + 0] = source[column * 4 + 2];
                target[column * 4 + 1] = source[column * 4 + 1];
                target[column * 4 + 2] = source[column * 4 + 0];
                target[column * 4 + 3] = 0;
            }
        } else {
            int rgb = c->format == SPA_VIDEO_FORMAT_RGB;
            for (uint32_t column = 0; column < c->width; ++column) {
                target[column * 4 + 0] = source[column * 3 + (rgb ? 2 : 0)];
                target[column * 4 + 1] = source[column * 3 + 1];
                target[column * 4 + 2] = source[column * 3 + (rgb ? 0 : 2)];
                target[column * 4 + 3] = 0;
            }
        }
    }

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

static void on_process(void *userdata)
{
    struct capture *c = userdata;
    struct pw_buffer *buffer = pw_stream_dequeue_buffer(c->stream);
    if (buffer == NULL)
        return;
    struct spa_buffer *buf = buffer->buffer;

    if (buf->n_datas >= 1 && c->shm != NULL) {
        struct spa_data *plane = &buf->datas[0];
        uint8_t *pixels = NULL;
        void *mapping = MAP_FAILED;
        size_t mapping_size = 0;
        int synchronized = 0;
        int dmabuf = plane->type == SPA_DATA_DmaBuf;

        if (plane->chunk == NULL || plane->chunk->stride < 0 ||
            (plane->chunk->flags & (SPA_CHUNK_FLAG_CORRUPTED | SPA_CHUNK_FLAG_EMPTY)))
            goto done;
        uint32_t row_bytes = c->width *
            (c->format == SPA_VIDEO_FORMAT_BGR || c->format == SPA_VIDEO_FORMAT_RGB ? 3u : 4u);
        uint32_t stride = plane->chunk->stride > 0 ?
            (uint32_t)plane->chunk->stride : row_bytes;
        uint64_t span = (uint64_t)stride * (c->height - 1) + row_bytes;
        uint32_t data_offset = plane->chunk->offset;
        if (stride < row_bytes || span > 256u * 1024 * 1024)
            goto done;
        if (dmabuf) {
            // DMA-BUF maxsize/chunk size may be zero or placeholders. Query
            // the fd and include BOTH the plane mapping and chunk offsets.
            if (!c->dmabuf_usable || plane->fd < 0)
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
            if (c->counter == 0)
                fprintf(stderr, "capture buffer: %s, source stride %u, chunk offset %u\n",
                    dmabuf ? "linear DMA-BUF" : "shared memory", stride, data_offset);
            publish_frame(c, pixels, stride, c->height);
        }
done:
        if (synchronized)
            (void)sync_dmabuf((int)plane->fd, DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ);
        if (mapping != MAP_FAILED)
            munmap(mapping, mapping_size);
    }

    pw_stream_queue_buffer(c->stream, buffer);
}

static void on_param_changed(void *userdata, uint32_t id,
                             const struct spa_pod *param)
{
    struct capture *c = userdata;
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
        fprintf(stderr, "negotiated unexpected format %d\n", info.format);
        return;
    }

    if (info.size.width == 0 || info.size.height == 0 ||
        info.size.width > 16384 || info.size.height > 16384 ||
        (uint64_t)info.size.width * 4 * info.size.height > 256u * 1024 * 1024) {
        fprintf(stderr, "unsupported capture geometry\n");
        c->exit_status = 1;
        pw_main_loop_quit(c->loop);
        return;
    }
    c->width = (uint32_t)info.size.width;
    c->height = (uint32_t)info.size.height;
    c->format = info.format;
    c->stride = SPA_ROUND_UP_N(c->width * 4, 4);
    /* The DMA-BUF path mmaps the fd directly, which is only correct for a
     * linear layout.  A tiled modifier needs a real importer (GBM/EGL), so
     * refuse to publish garbage rather than emit a corrupt frame. */
    c->dmabuf_usable = spa_pod_find_prop(param, NULL, SPA_FORMAT_VIDEO_modifier) != NULL &&
                       info.modifier == DRM_FORMAT_MOD_LINEAR;
    fprintf(stderr, "negotiated: %ux%u stride %u modifier %#llx%s\n", c->width,
            c->height, c->stride, (unsigned long long)info.modifier,
            c->dmabuf_usable ? "" : " (DMA-BUF CPU mapping disabled)");

    const struct frame_header *header = (const struct frame_header *)c->shm;
    if (header == NULL || header->width != c->width || header->height != c->height) {
        if (shm_setup(c) < 0) {
            c->exit_status = 1;
            pw_main_loop_quit(c->loop);
        }
    }
}

static void on_state_changed(void *userdata, enum pw_stream_state old,
                             enum pw_stream_state state, const char *error)
{
    struct capture *c = userdata;
    fprintf(stderr, "stream state: %s%s%s\n",
            pw_stream_state_as_string(state),
            error != NULL && error[0] != '\0' ? " - " : "",
            error != NULL ? error : "");
    if (c->stopping)
        return;
    if (state == PW_STREAM_STATE_STREAMING) {
        c->was_streaming = 1;
        c->paused_ms = 0;
    } else if (state == PW_STREAM_STATE_PAUSED && c->was_streaming) {
        if (c->paused_ms == 0)
            c->paused_ms = monotonic_ms();
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
    // A static desktop can legitimately produce no new frames while streaming.
    // Only a prolonged PAUSED state or a stream with no FIRST frame is fatal.
    if (!c->stopping &&
        ((c->paused_ms != 0 && now - c->paused_ms >= 10000) ||
         (c->counter == 0 && now - c->started_ms >= 30000))) {
        fprintf(stderr, "capture stream stopped producing usable frames; reconnecting\n");
        c->exit_status = 75;
        pw_main_loop_quit(c->loop);
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

int main(void)
{
    const char *fd_text = getenv("UUR_PW_FD");
    const char *node_text = getenv("UUR_PW_NODE");
    if (fd_text == NULL || node_text == NULL) {
        fprintf(stderr,
                "UUR_PW_FD and UUR_PW_NODE must be set by 'uur capture'\n");
        return 2;
    }
    const char *frame_path = getenv("UUR_FRAME_PATH");
    if (frame_path == NULL || frame_path[0] != '/' ||
        strlen(frame_path) >= PATH_MAX) {
        fprintf(stderr, "UUR_FRAME_PATH must be a short absolute path\n");
        return 2;
    }
    int pw_fd = atoi(fd_text);
    uint32_t node_id = (uint32_t)strtoul(node_text, NULL, 10);

    pw_init(NULL, NULL);

    struct capture c;
    memset(&c, 0, sizeof(c));
    c.shm_fd = -1;
    memcpy(c.frame_path, frame_path, strlen(frame_path) + 1);
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

    /* Connect through the portal's private PipeWire instance: only the
     * consented node is reachable on this descriptor. */
    struct pw_core *core = pw_context_connect_fd(context, pw_fd, NULL, 0);
    if (core == NULL) {
        fprintf(stderr, "pw_context_connect_fd failed\n");
        return 1;
    }
    struct spa_hook core_listener;
    pw_core_add_listener(core, &core_listener, &core_events, &c);

    struct pw_properties *props = pw_properties_new(
        PW_KEY_MEDIA_TYPE, "Video", PW_KEY_MEDIA_CATEGORY, "Capture",
        PW_KEY_MEDIA_ROLE, "Screen", NULL);
    c.stream = pw_stream_new(core, "uur-capture", props);
    if (c.stream == NULL) {
        fprintf(stderr, "pw_stream_new failed\n");
        return 1;
    }
    pw_stream_add_listener(c.stream, &c.listener, &stream_events, &c);

    uint8_t pod_buffer[1024];
    struct spa_pod_builder pod_builder =
        SPA_POD_BUILDER_INIT(pod_buffer, sizeof(pod_buffer));
    const struct spa_pod *params[2];
    params[0] = capture_format(&pod_builder, 1);
    params[1] = capture_format(&pod_builder, 0);
    c.started_ms = monotonic_ms();

    if (pw_stream_connect(c.stream, PW_DIRECTION_INPUT, node_id,
                          PW_STREAM_FLAG_MAP_BUFFERS |
                              PW_STREAM_FLAG_AUTOCONNECT,
                          params, 2) < 0) {
        fprintf(stderr, "pw_stream_connect failed\n");
        return 1;
    }

    struct spa_source *watchdog = pw_loop_add_timer(pw_main_loop_get_loop(loop),
                                                    on_watchdog, &c);
    struct timespec interval = {.tv_sec = 1};
    if (watchdog == NULL || pw_loop_update_timer(pw_main_loop_get_loop(loop),
            watchdog, &interval, &interval, false) < 0) {
        fprintf(stderr, "could not start capture watchdog\n");
        return 1;
    }
    pw_main_loop_run(loop);

    c.stopping = 1;
    active_capture = NULL;
    pw_loop_destroy_source(pw_main_loop_get_loop(loop), watchdog);
    pw_stream_destroy(c.stream);
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
    unlink(c.frame_path);
    pw_deinit();
    return c.exit_status;
}
