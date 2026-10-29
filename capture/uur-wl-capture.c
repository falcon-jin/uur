/* Direct Wayland monitor capture, explicitly enabled by the user.
 * Uses compositor-provided wl_shm buffers; no GBM, portal or GPU import is
 * required. Frames use the same private UURF transport as PipeWire capture.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <poll.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>
#include <wayland-client.h>
#include "wlr-screencopy-unstable-v1-client-protocol.h"
#include "../hook/frame-protocol.h"

struct frame_header {
    uint32_t magic, version, width, height, stride, format;
    _Atomic uint64_t total_frames;
    uint32_t flags, monitor_count;
    uint8_t reserved[24];
    struct uurf_monitor_layout monitors[UURF_MAX_MONITORS];
};
_Static_assert(sizeof(struct frame_header) == UURF_HEADER_BYTES, "frame header ABI");

struct output {
    struct wl_output *proxy;
    uint32_t global;
    char name[128];
    int32_t width, height;
};

struct capture {
    struct wl_display *display;
    struct wl_registry *registry;
    struct wl_shm *shm;
    struct zwlr_screencopy_manager_v1 *manager;
    struct output outputs[64];
    size_t output_count;
    struct output *selected;
    struct zwlr_screencopy_frame_v1 *frame;
    struct wl_buffer *buffer;
    void *pixels;
    size_t pixels_size;
    uint32_t width, height, stride, format, flags;
    int done, failed;
    int ring_fd;
    void *ring;
    size_t ring_size;
    uint32_t ring_width, ring_height;
    uint64_t counter;
    const char *frame_path;
};

static volatile sig_atomic_t stopping;
static void stop_capture(int signum) { (void)signum; stopping = 1; }

static void output_geometry(void *data, struct wl_output *output, int32_t x,
    int32_t y, int32_t physical_width, int32_t physical_height, int32_t subpixel,
    const char *make, const char *model, int32_t transform)
{
    (void)data; (void)output; (void)x; (void)y; (void)physical_width;
    (void)physical_height; (void)subpixel; (void)make; (void)model; (void)transform;
}
static void output_mode(void *data, struct wl_output *output, uint32_t flags,
    int32_t width, int32_t height, int32_t refresh)
{
    (void)output; (void)refresh;
    struct output *o = data;
    if (flags & WL_OUTPUT_MODE_CURRENT) { o->width = width; o->height = height; }
}
static void output_done(void *data, struct wl_output *output) { (void)data; (void)output; }
static void output_scale(void *data, struct wl_output *output, int32_t scale)
{ (void)data; (void)output; (void)scale; }
static void output_name(void *data, struct wl_output *output, const char *name)
{ (void)output; snprintf(((struct output *)data)->name, 128, "%s", name); }
static void output_description(void *data, struct wl_output *output, const char *description)
{ (void)data; (void)output; (void)description; }
static const struct wl_output_listener output_listener = {
    .geometry = output_geometry, .mode = output_mode, .done = output_done,
    .scale = output_scale, .name = output_name, .description = output_description,
};

static void registry_global(void *data, struct wl_registry *registry, uint32_t name,
    const char *interface, uint32_t version)
{
    struct capture *c = data;
    if (!strcmp(interface, wl_shm_interface.name)) {
        c->shm = wl_registry_bind(registry, name, &wl_shm_interface, 1);
    } else if (!strcmp(interface, zwlr_screencopy_manager_v1_interface.name)) {
        c->manager = wl_registry_bind(registry, name,
            &zwlr_screencopy_manager_v1_interface, version < 3 ? version : 3);
    } else if (!strcmp(interface, wl_output_interface.name) && c->output_count < 64) {
        struct output *o = &c->outputs[c->output_count++];
        o->global = name;
        o->proxy = wl_registry_bind(registry, name, &wl_output_interface, version < 4 ? version : 4);
        wl_output_add_listener(o->proxy, &output_listener, o);
    }
}
static void registry_remove(void *data, struct wl_registry *registry, uint32_t name)
{
    (void)registry;
    struct capture *c = data;
    if (c->selected != NULL && c->selected->global == name) {
        fprintf(stderr, "captured output disconnected\n");
        c->failed = 1; stopping = 1;
    }
}
static const struct wl_registry_listener registry_listener = {
    .global = registry_global, .global_remove = registry_remove,
};

static int setup_ring(struct capture *c)
{
    if (c->ring != NULL) {
        if (c->ring_width == c->width && c->ring_height == c->height) return 0;
        fprintf(stderr, "capture geometry changed; restart uur run\n");
        return -1;
    }
    size_t bytes = (size_t)c->width * 4 * c->height;
    c->ring_size = UURF_HEADER_BYTES + UURF_SLOT_COUNT * (sizeof(uint64_t) + bytes);
    c->ring_fd = open(c->frame_path, O_RDWR | O_CREAT | O_TRUNC | O_CLOEXEC | O_NOFOLLOW, 0600);
    if (c->ring_fd < 0 || fchmod(c->ring_fd, 0600) < 0 ||
        ftruncate(c->ring_fd, (off_t)c->ring_size) < 0) { perror("frame transport"); return -1; }
    c->ring = mmap(NULL, c->ring_size, PROT_READ | PROT_WRITE, MAP_SHARED, c->ring_fd, 0);
    if (c->ring == MAP_FAILED) { c->ring = NULL; perror("frame mmap"); return -1; }
    c->ring_width = c->width; c->ring_height = c->height;
    struct frame_header *header = c->ring;
    memset(header, 0, sizeof(*header));
    header->magic = UURF_MAGIC; header->version = UURF_VERSION;
    header->width = c->width; header->height = c->height;
    header->stride = c->width * 4; header->format = 1;
    atomic_init(&header->total_frames, 0);
    header->flags = 0;
    header->monitor_count = 1;
    header->monitors[0].logical_x = 0;
    header->monitors[0].logical_y = 0;
    header->monitors[0].logical_width = c->width;
    header->monitors[0].logical_height = c->height;
    header->monitors[0].frame_x = 0;
    header->monitors[0].frame_y = 0;
    header->monitors[0].frame_width = c->width;
    header->monitors[0].frame_height = c->height;
    header->monitors[0].capture_width = c->width;
    header->monitors[0].capture_height = c->height;
    for (uint32_t i = 0; i < UURF_SLOT_COUNT; ++i) {
        _Atomic uint64_t *seq = (_Atomic uint64_t *)((uint8_t *)c->ring +
            UURF_HEADER_BYTES + i * (sizeof(uint64_t) + bytes));
        atomic_init(seq, 0);
    }
    fprintf(stderr, "Wayland output %s: %ux%u\n", c->selected->name, c->width, c->height);
    return 0;
}

static int supported_format(uint32_t format)
{
    return format == WL_SHM_FORMAT_XRGB8888 || format == WL_SHM_FORMAT_ARGB8888 ||
        format == WL_SHM_FORMAT_XBGR8888 || format == WL_SHM_FORMAT_ABGR8888 ||
        format == WL_SHM_FORMAT_XRGB2101010 || format == WL_SHM_FORMAT_ARGB2101010 ||
        format == WL_SHM_FORMAT_XBGR2101010 || format == WL_SHM_FORMAT_ABGR2101010;
}
static int submit_buffer(struct capture *c)
{
    if (!supported_format(c->format) || c->width == 0 || c->height == 0 ||
        c->width > 16384 || c->height > 16384 || c->stride < c->width * 4 ||
        (uint64_t)c->stride * c->height > 256u * 1024 * 1024) {
        fprintf(stderr, "unsupported Wayland SHM format or geometry\n");
        return -1;
    }
    if (setup_ring(c) < 0) return -1;
    c->pixels_size = (size_t)c->stride * c->height;
    int fd = memfd_create("uur-wayland-frame", MFD_CLOEXEC);
    if (fd < 0) { perror("memfd_create"); return -1; }
    if (ftruncate(fd, (off_t)c->pixels_size) < 0) { close(fd); return -1; }
    c->pixels = mmap(NULL, c->pixels_size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (c->pixels == MAP_FAILED) { c->pixels = NULL; close(fd); return -1; }
    struct wl_shm_pool *pool = wl_shm_create_pool(c->shm, fd, (int32_t)c->pixels_size);
    c->buffer = wl_shm_pool_create_buffer(pool, 0, (int32_t)c->width,
        (int32_t)c->height, (int32_t)c->stride, c->format);
    wl_shm_pool_destroy(pool); close(fd);
    zwlr_screencopy_frame_v1_copy(c->frame, c->buffer);
    return 0;
}
static void frame_buffer(void *data, struct zwlr_screencopy_frame_v1 *frame,
    uint32_t format, uint32_t width, uint32_t height, uint32_t stride)
{
    struct capture *c = data;
    c->format = format; c->width = width; c->height = height; c->stride = stride;
    if (zwlr_screencopy_frame_v1_get_version(frame) < 3 && submit_buffer(c) < 0) {
        c->done = 1; c->failed = 1;
    }
}
static void frame_buffer_done(void *data, struct zwlr_screencopy_frame_v1 *frame)
{
    (void)frame;
    struct capture *c = data;
    if (submit_buffer(c) < 0) { c->done = 1; c->failed = 1; }
}
static void frame_flags(void *data, struct zwlr_screencopy_frame_v1 *frame, uint32_t flags)
{ (void)frame; ((struct capture *)data)->flags = flags; }
static void frame_ready(void *data, struct zwlr_screencopy_frame_v1 *frame,
    uint32_t hi, uint32_t lo, uint32_t ns)
{ (void)frame; (void)hi; (void)lo; (void)ns; ((struct capture *)data)->done = 1; }
static void frame_failed(void *data, struct zwlr_screencopy_frame_v1 *frame)
{ (void)frame; struct capture *c = data; c->done = 1; c->failed = 1; }
static void frame_damage(void *data, struct zwlr_screencopy_frame_v1 *frame,
    uint32_t x, uint32_t y, uint32_t width, uint32_t height)
{ (void)data; (void)frame; (void)x; (void)y; (void)width; (void)height; }
static void frame_dmabuf(void *data, struct zwlr_screencopy_frame_v1 *frame,
    uint32_t format, uint32_t width, uint32_t height)
{ (void)data; (void)frame; (void)format; (void)width; (void)height; }
static const struct zwlr_screencopy_frame_v1_listener frame_listener = {
    .buffer = frame_buffer, .flags = frame_flags, .ready = frame_ready,
    .failed = frame_failed, .damage = frame_damage,
    .linux_dmabuf = frame_dmabuf, .buffer_done = frame_buffer_done,
};

static void publish_frame(struct capture *c)
{
    size_t bytes = (size_t)c->width * 4 * c->height;
    uint64_t n = ++c->counter;
    _Atomic uint64_t *seq = (_Atomic uint64_t *)((uint8_t *)c->ring +
        uurf_slot_offset(n, sizeof(uint64_t) + bytes));
    uint8_t *dest = (uint8_t *)seq + sizeof(uint64_t);
    atomic_store_explicit(seq, 2 * n - 1, memory_order_release);
    int rgb = c->format == WL_SHM_FORMAT_XBGR8888 || c->format == WL_SHM_FORMAT_ABGR8888;
    int ten = c->format == WL_SHM_FORMAT_XRGB2101010 || c->format == WL_SHM_FORMAT_ARGB2101010 ||
        c->format == WL_SHM_FORMAT_XBGR2101010 || c->format == WL_SHM_FORMAT_ABGR2101010;
    int ten_bgr = c->format == WL_SHM_FORMAT_XBGR2101010 || c->format == WL_SHM_FORMAT_ABGR2101010;
    for (uint32_t y = 0; y < c->height; ++y) {
        uint32_t row = c->flags & ZWLR_SCREENCOPY_FRAME_V1_FLAGS_Y_INVERT ? c->height - y - 1 : y;
        const uint8_t *src = (const uint8_t *)c->pixels + (size_t)row * c->stride;
        uint8_t *dst = dest + (size_t)y * c->width * 4;
        for (uint32_t x = 0; x < c->width; ++x, src += 4, dst += 4) {
            if (ten) {
                uint32_t value; memcpy(&value, src, 4);
                dst[0] = (uint8_t)((((value >> (ten_bgr ? 20 : 0)) & 1023) * 255 + 511) / 1023);
                dst[1] = (uint8_t)((((value >> 10) & 1023) * 255 + 511) / 1023);
                dst[2] = (uint8_t)((((value >> (ten_bgr ? 0 : 20)) & 1023) * 255 + 511) / 1023);
            } else {
                dst[0] = src[rgb ? 2 : 0]; dst[1] = src[1]; dst[2] = src[rgb ? 0 : 2];
            }
            dst[3] = 0;
        }
    }
    atomic_store_explicit(seq, 2 * n, memory_order_release);
    atomic_store_explicit(&((struct frame_header *)c->ring)->total_frames, n, memory_order_release);
}

static int dispatch(struct capture *c)
{
    while (wl_display_prepare_read(c->display) != 0) {
        if (wl_display_dispatch_pending(c->display) < 0) return -1;
        if (c->done || c->failed || stopping) return 0;
    }
    if (wl_display_flush(c->display) < 0 && errno != EAGAIN) {
        wl_display_cancel_read(c->display); return -1;
    }
    struct pollfd fd = { .fd = wl_display_get_fd(c->display), .events = POLLIN };
    int rc = poll(&fd, 1, 10000);
    if (rc <= 0 || !(fd.revents & POLLIN)) {
        wl_display_cancel_read(c->display);
        if (rc == 0) fprintf(stderr, "Wayland capture frame timed out\n");
        return stopping ? 0 : -1;
    }
    if (wl_display_read_events(c->display) < 0) return -1;
    return wl_display_dispatch_pending(c->display);
}

static void clear_frame(struct capture *c)
{
    if (c->frame != NULL) { zwlr_screencopy_frame_v1_destroy(c->frame); c->frame = NULL; }
    if (c->buffer != NULL) { wl_buffer_destroy(c->buffer); c->buffer = NULL; }
    if (c->pixels != NULL) { munmap(c->pixels, c->pixels_size); c->pixels = NULL; }
    c->pixels_size = 0; c->width = c->height = c->stride = c->format = c->flags = 0;
}

int main(int argc, char **argv)
{
    struct capture c = { .ring_fd = -1 };
    int list = argc == 2 && !strcmp(argv[1], "--list-outputs");
    if (argc > 1 && !list) { fprintf(stderr, "usage: uur-wl-capture [--list-outputs]\n"); return 2; }
    c.frame_path = getenv("UUR_FRAME_PATH");
    if (!list && (c.frame_path == NULL || c.frame_path[0] != '/')) {
        fprintf(stderr, "UUR_FRAME_PATH must name the private absolute frame transport\n"); return 2;
    }
    struct sigaction sa = { .sa_handler = stop_capture };
    sigemptyset(&sa.sa_mask); sigaction(SIGINT, &sa, NULL); sigaction(SIGTERM, &sa, NULL);
    c.display = wl_display_connect(NULL);
    if (c.display == NULL) { fprintf(stderr, "cannot connect to Wayland display\n"); return 1; }
    c.registry = wl_display_get_registry(c.display);
    wl_registry_add_listener(c.registry, &registry_listener, &c);
    if (wl_display_roundtrip(c.display) < 0 || wl_display_roundtrip(c.display) < 0) {
        c.failed = 1; goto cleanup;
    }
    if (list) {
        for (size_t i = 0; i < c.output_count; ++i) {
            printf("%s %dx%d\n", c.outputs[i].name, c.outputs[i].width, c.outputs[i].height);
        }
        goto cleanup;
    }
    if (c.manager == NULL || c.shm == NULL) {
        fprintf(stderr, "compositor does not expose wlr-screencopy with wl_shm buffers\n");
        c.failed = 1; goto cleanup;
    }
    const char *name = getenv("UUR_CAPTURE_OUTPUT");
    if (name != NULL && name[0] != '\0') {
        for (size_t i = 0; i < c.output_count; ++i) {
            if (!strcmp(name, c.outputs[i].name)) c.selected = &c.outputs[i];
        }
    } else if (c.output_count == 1) { c.selected = &c.outputs[0]; }
    if (c.selected == NULL) {
        fprintf(stderr, "select one output with uur display backend wayland --output NAME:\n");
        for (size_t i = 0; i < c.output_count; ++i) fprintf(stderr, "  %s\n", c.outputs[i].name);
        c.failed = 1; goto cleanup;
    }
    while (!stopping && !c.failed) {
        c.done = 0;
        c.frame = zwlr_screencopy_manager_v1_capture_output(c.manager, 1, c.selected->proxy);
        zwlr_screencopy_frame_v1_add_listener(c.frame, &frame_listener, &c);
        while (!c.done && !stopping) {
            if (dispatch(&c) < 0) { c.failed = 1; break; }
        }
        if (c.done && !c.failed && !stopping) publish_frame(&c);
        clear_frame(&c);
        struct timespec delay = { .tv_nsec = 33333333 };
        nanosleep(&delay, NULL);
    }
cleanup:
    clear_frame(&c);
    if (c.ring != NULL) munmap(c.ring, c.ring_size);
    if (c.ring_fd >= 0) {
        struct stat created, present;
        if (fstat(c.ring_fd, &created) == 0 && lstat(c.frame_path, &present) == 0 &&
            created.st_dev == present.st_dev && created.st_ino == present.st_ino) unlink(c.frame_path);
        close(c.ring_fd);
    }
    for (size_t i = 0; i < c.output_count; ++i) wl_output_destroy(c.outputs[i].proxy);
    if (c.manager != NULL) zwlr_screencopy_manager_v1_destroy(c.manager);
    if (c.shm != NULL) wl_shm_destroy(c.shm);
    wl_registry_destroy(c.registry); wl_display_disconnect(c.display);
    return c.failed ? 1 : 0;
}
