/*
 * uur-hook — input reroute for NetEase UU Remote under Wine.
 *
 * Loaded into GameViewerServer.exe through the wevtapi.dll preload chain
 * (see wevtapi.c).  At PROCESS_ATTACH it rewrites the SendInput entries of
 * every loaded module's import table to point at hook_SendInput, which
 * forwards each input record over an authenticated loopback socket to the
 * uur host daemon.  The daemon injects the record onto the real desktop
 * (XTest on X11, RemoteDesktop portal on Wayland).
 *
 * Failure policy: if the socket is unavailable, hook_SendInput falls back
 * to the original SendInput so the client keeps working with input
 * contained inside the managed Wine prefix.
 *
 * Wire format mirrors src/protocol.rs: 16-byte record
 *   u32 kind (1 keyboard, 2 mouse) u16 code u16 state i32 a i32 b
 */

#define WIN32_LEAN_AND_MEAN
#ifndef _WIN32_WINNT
#define _WIN32_WINNT 0x0601
#endif

#include <winsock2.h>
#include <ws2tcpip.h>
#include <windows.h>
#include <tlhelp32.h>
#include <dxgi.h>

#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include "cursor-overlay.h"
#include "frame-protocol.h"

#define UUR_MAGIC 0x50495555u
#define UUR_VERSION 1u
#define UUR_RECORD_HELLO 1u
#define UUR_RECORD_MOUSE 2u
#define UUR_RECORD_KEYBOARD 3u

static UINT(WINAPI *original_send_input)(UINT, LPINPUT, int);
static HRESULT(WINAPI *original_create_dxgi_factory1)(REFIID, void **);
static FARPROC(WINAPI *original_get_proc_address)(HMODULE, LPCSTR);
UINT WINAPI hook_send_input(UINT count, LPINPUT inputs, int size);

static volatile LONG dxgi_factories_patched = 0;
static volatile LONG dxgi_adapters_patched = 0;
static volatile LONG dxgi_outputs_patched = 0;
static volatile LONG dxgi_duplicate_calls = 0;
static volatile LONG dxgi_dup_acquires = 0;
static volatile LONG dxgi_dup_frames = 0;

static HRESULT __attribute__((unused))(STDMETHODCALLTYPE *original_dxgi_acquire_next_frame)(
    void *duplication, UINT timeout, void *frame_info, void **resource) = NULL;
/* referenced by patch_dxgi_duplication below */

/* ---- DXGI duplication disable ----------------------------------------- */

/* The client's primary capture is IDXGIOutput1::DuplicateOutput.  Wine's
 * implementation succeeds but only delivers the empty XWayland desktop
 * (black video).  Forcing DuplicateOutput to fail makes the client fall
 * back to its GDI capture path, which the frame feed below supplies. */

static const GUID uur_iid_idxgioutput1 = {
    0x00cddea8, 0x939b, 0x4b83,
    {0xa3, 0x40, 0xa6, 0x85, 0x22, 0x66, 0x66, 0xcc}
};

/* Minimal IDXGIOutput1 shape: mingw's C-mode dxgi.h lacks it.  The
 * vtable is IUnknown(3) + IDXGIOutput(12) methods, then DuplicateOutput. */
typedef struct uur_idxgioutput1_vtbl {
    void *methods[15];
    HRESULT(STDMETHODCALLTYPE *DuplicateOutput)(void *, IUnknown *);
} uur_idxgioutput1_vtbl;
typedef struct uur_idxgioutput1 {
    uur_idxgioutput1_vtbl *lpVtbl;
} uur_idxgioutput1;

#ifndef DXGI_ERROR_UNSUPPORTED
#define DXGI_ERROR_UNSUPPORTED ((HRESULT)0x887A0004)
#endif

static HRESULT(STDMETHODCALLTYPE *original_dxgi_enum_adapters1)(
    IDXGIFactory1 *factory, UINT index, IDXGIAdapter1 **adapter) = NULL;
static HRESULT(STDMETHODCALLTYPE *original_dxgi_enum_outputs)(
    IDXGIAdapter *adapter, UINT index, IDXGIOutput **output) = NULL;
static HRESULT STDMETHODCALLTYPE hook_dxgi_enum_adapters1(
    IDXGIFactory1 *factory, UINT index, IDXGIAdapter1 **adapter);
static HRESULT STDMETHODCALLTYPE hook_dxgi_enum_outputs(
    IDXGIAdapter *adapter, UINT index, IDXGIOutput **output);
static HRESULT WINAPI hook_dxgi_duplicate_output(uur_idxgioutput1 *output,
                                                 IUnknown *device);

static int patch_vtable_slot(void **slot, void *hook, void **original)
{
    DWORD old_protect = 0;
    if (*slot == hook)
        return 1;
    if (original != NULL)
        *original = *slot;
    if (!VirtualProtect(slot, sizeof(void *), PAGE_READWRITE, &old_protect))
        return 0;
    *slot = hook;
    VirtualProtect(slot, sizeof(void *), old_protect, &old_protect);
    return 1;
}

static HRESULT(STDMETHODCALLTYPE *original_dxgi_enum_adapters)(
    IDXGIFactory *factory, UINT index, IDXGIAdapter **adapter) = NULL;
static void patch_dxgi_adapter(IDXGIAdapter *adapter);

static void patch_dxgi_output(IDXGIOutput *output)
{
    uur_idxgioutput1 *output1 = NULL;

    if (output == NULL || output->lpVtbl == NULL)
        return;
    if (output->lpVtbl->QueryInterface(
            output, &uur_iid_idxgioutput1, (void **)&output1) != S_OK)
        return;
    if (output1 != NULL && output1->lpVtbl != NULL) {
        if (patch_vtable_slot((void **)&output1->lpVtbl->DuplicateOutput,
                              (void *)hook_dxgi_duplicate_output, NULL)) {
            InterlockedIncrement(&dxgi_outputs_patched);
        }
        /* release through the known-good base interface */
        output->lpVtbl->Release(output);
    }
}

static HRESULT STDMETHODCALLTYPE hook_dxgi_enum_adapters(
    IDXGIFactory *factory, UINT index, IDXGIAdapter **adapter)
{
    HRESULT result = original_dxgi_enum_adapters
        ? original_dxgi_enum_adapters(factory, index, adapter)
        : DXGI_ERROR_INVALID_CALL;

    if (result == S_OK && adapter != NULL && *adapter != NULL)
        patch_dxgi_adapter(*adapter);
    return result;
}

static void patch_dxgi_adapter(IDXGIAdapter *adapter)
{
    if (adapter == NULL || adapter->lpVtbl == NULL)
        return;
    if (patch_vtable_slot((void **)&adapter->lpVtbl->EnumOutputs,
                          (void *)hook_dxgi_enum_outputs,
                          (void **)&original_dxgi_enum_outputs)) {
        InterlockedIncrement(&dxgi_adapters_patched);
    }
}

static HRESULT STDMETHODCALLTYPE hook_dxgi_enum_adapters1(
    IDXGIFactory1 *factory, UINT index, IDXGIAdapter1 **adapter)
{
    HRESULT result = original_dxgi_enum_adapters1
        ? original_dxgi_enum_adapters1(factory, index, adapter)
        : DXGI_ERROR_INVALID_CALL;

    if (result == S_OK && adapter != NULL && *adapter != NULL)
        patch_dxgi_adapter((IDXGIAdapter *)*adapter);
    return result;
}

static HRESULT STDMETHODCALLTYPE hook_dxgi_enum_outputs(
    IDXGIAdapter *adapter, UINT index, IDXGIOutput **output)
{
    HRESULT result = original_dxgi_enum_outputs
        ? original_dxgi_enum_outputs(adapter, index, output)
        : DXGI_ERROR_INVALID_CALL;

    if (result == S_OK && output != NULL && *output != NULL)
        patch_dxgi_output(*output);
    return result;
}

static HRESULT WINAPI hook_dxgi_duplicate_output(uur_idxgioutput1 *output,
                                                 IUnknown *device)
{
    (void)output;
    (void)device;
    InterlockedIncrement(&dxgi_duplicate_calls);
    /* Wine's duplication succeeds but delivers the empty XWayland desktop.
     * Failing it sends the client to its GDI capture path. */
    return DXGI_ERROR_UNSUPPORTED;
}

static void patch_dxgi_factory(IDXGIFactory1 *factory)
{
    if (factory == NULL || factory->lpVtbl == NULL)
        return;
    patch_vtable_slot((void **)&factory->lpVtbl->EnumAdapters1,
                      (void *)hook_dxgi_enum_adapters1,
                      (void **)&original_dxgi_enum_adapters1);
    patch_vtable_slot((void **)&factory->lpVtbl->EnumAdapters,
                      (void *)hook_dxgi_enum_adapters,
                      (void **)&original_dxgi_enum_adapters);
}

static HRESULT WINAPI hook_create_dxgi_factory1(REFIID interface_id,
                                                void **factory)
{
    HRESULT result = original_create_dxgi_factory1 != NULL
        ? original_create_dxgi_factory1(interface_id, factory)
        : DXGI_ERROR_INVALID_CALL;

    if (result == S_OK && factory != NULL && *factory != NULL) {
        IDXGIFactory1 *f = (IDXGIFactory1 *)*factory;
        if (f->lpVtbl != NULL) {
            patch_dxgi_factory(f);
            InterlockedIncrement(&dxgi_factories_patched);
        }
    }
    return result;
}
static BOOL(WINAPI *original_bit_blt)(HDC, int, int, int, int, HDC, int, int,
                                      DWORD);
static BOOL(WINAPI *original_stretch_blt)(HDC, int, int, int, int, HDC, int,
                                          int, int, int, DWORD);
static volatile LONG capture_calls = 0;
static volatile LONG capture_rendered = 0;
static volatile LONG capture_fallbacks = 0;
static volatile LONG iat_hooks_installed = 0;

static SOCKET hook_socket = INVALID_SOCKET;
static volatile LONG hook_busy = 0;

/* ---- capture frame feed (docs/capture-protocol.md) -------------------- */

static void *frame_map = NULL;
static HANDLE frame_map_handle = NULL;
static uint32_t frame_width;
static uint32_t frame_height;
static uint32_t frame_stride;
static uint64_t frame_slot_bytes;
static unsigned char *frame_snapshot;
static size_t frame_snapshot_capacity;
static size_t frame_map_size;
static BY_HANDLE_FILE_INFORMATION frame_identity;
static ULONGLONG frame_checked_at;
static SRWLOCK frame_lock = SRWLOCK_INIT;

static char *read_ini_value(const char *path, const char *key);

static int resolve_frame_path(WCHAR *path, int capacity)
{
    DWORD count = GetEnvironmentVariableW(L"UUR_FRAME_PATH_WIN", path,
                                          (DWORD)capacity);
    if (count > 0 && count < (DWORD)capacity)
        return 1;

    char *utf8 = read_ini_value("C:\\uur-bridge.ini", "frame_path");
    if (utf8 == NULL)
        return 0;
    int converted = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, utf8,
                                        -1, path, capacity);
    free(utf8);
    return converted > 0;
}

static uint32_t load_u32(const unsigned char *p)
{
    return (uint32_t)p[0] | ((uint32_t)p[1] << 8) | ((uint32_t)p[2] << 16) |
           ((uint32_t)p[3] << 24);
}

static uint64_t load_u64(const unsigned char *p)
{
    return load_u32(p) | ((uint64_t)load_u32(p + 4) << 32);
}

static void clear_frame_map(void)
{
    if (frame_map != NULL)
        UnmapViewOfFile(frame_map);
    if (frame_map_handle != NULL)
        CloseHandle(frame_map_handle);
    frame_map = NULL;
    frame_map_handle = NULL;
    frame_map_size = 0;
}

/* Producers publish immutable geometry via atomic replacement. Re-check the
 * file identity periodically, including after an ungraceful helper restart. */
static int ensure_frame_map(void)
{
    HANDLE file;
    void *view;
    const unsigned char *header;
    LARGE_INTEGER size;
    BY_HANDLE_FILE_INFORMATION identity;
    WCHAR path[32768];
    ULONGLONG now = GetTickCount64();

    if (frame_map != NULL && now - frame_checked_at < 250 &&
        load_u64((const unsigned char *)frame_map + 24) != 0)
        return 1;

    if (!resolve_frame_path(path, (int)(sizeof(path) / sizeof(path[0]))))
        return 0;
    file = CreateFileW(path, GENERIC_READ,
                       FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, NULL, OPEN_EXISTING,
                       0, NULL);
    if (file == INVALID_HANDLE_VALUE) {
        clear_frame_map();
        return 0;
    }
    if (!GetFileInformationByHandle(file, &identity) ||
        !GetFileSizeEx(file, &size) || size.QuadPart < UURF_HEADER_BYTES ||
        (uint64_t)size.QuadPart > UURF_HEADER_BYTES +
            UURF_SLOT_COUNT * (8ull + 256ull * 1024 * 1024)) {
        CloseHandle(file);
        clear_frame_map();
        return 0;
    }
    frame_checked_at = now;
    if (frame_map != NULL &&
        identity.dwVolumeSerialNumber == frame_identity.dwVolumeSerialNumber &&
        identity.nFileIndexHigh == frame_identity.nFileIndexHigh &&
        identity.nFileIndexLow == frame_identity.nFileIndexLow &&
        (uint64_t)size.QuadPart == frame_map_size) {
        CloseHandle(file);
        return 1;
    }
    clear_frame_map();
    frame_map_handle = CreateFileMappingW(file, NULL, PAGE_READONLY, 0, 0,
                                          NULL);
    CloseHandle(file);
    if (frame_map_handle == NULL)
        return 0;
    view = MapViewOfFile(frame_map_handle, FILE_MAP_READ, 0, 0, 0);
    if (view == NULL) {
        clear_frame_map();
        return 0;
    }

    header = (const unsigned char *)view;
    if (load_u32(header) != UURF_MAGIC ||
        load_u32(header + 4) != UURF_VERSION || load_u32(header + 20) != 1u) {
        UnmapViewOfFile(view);
        CloseHandle(frame_map_handle);
        frame_map_handle = NULL;
        return 0;
    }
    frame_width = load_u32(header + 8);
    frame_height = load_u32(header + 12);
    frame_stride = load_u32(header + 16);
    frame_slot_bytes = 8ull + (uint64_t)frame_stride * frame_height;
    uint64_t expected = UURF_HEADER_BYTES + UURF_SLOT_COUNT * frame_slot_bytes;
    if (frame_width == 0 || frame_height == 0 ||
        frame_width > 16384 || frame_height > 16384 ||
        frame_stride != frame_width * 4 ||
        (uint64_t)frame_stride * frame_height > 256ull * 1024 * 1024 ||
        expected > (uint64_t)size.QuadPart) {
        UnmapViewOfFile(view);
        CloseHandle(frame_map_handle);
        frame_map_handle = NULL;
        return 0;
    }
    frame_map_size = (size_t)size.QuadPart;
    frame_map = view;
    frame_identity = identity;
    return 1;
}

static BOOL source_is_screen_dc(HDC source)
{
    return GetObjectType(source) == OBJ_DC &&
           GetDeviceCaps(source, TECHNOLOGY) == DT_RASDISPLAY;
}

/* GDI screen captures do not include the X11 hardware cursor.  Keep the
 * cursor visible in the fallback path used on X11 by compositing Wine's
 * current cursor onto the capture destination after the desktop blit. */
static void draw_cursor_overlay(HDC hdc_dest, int x, int y, int width, int height,
                                int source_x, int source_y, int source_width,
                                int source_height)
{
    CURSORINFO cursor;
    ICONINFO icon;
    BITMAP mask;
    POINT position;
    uur_cursor_rect source = {source_x, source_y, source_width, source_height};
    uur_cursor_rect destination = {x, y, width, height};
    uur_cursor_rect overlay;
    uur_cursor_shape shape;

    if (hdc_dest == NULL || width <= 0 || height <= 0 || source_width <= 0 ||
        source_height <= 0 || source_is_screen_dc(hdc_dest))
        return;
    ZeroMemory(&cursor, sizeof(cursor));
    cursor.cbSize = sizeof(cursor);
    if (!GetCursorInfo(&cursor) || !(cursor.flags & CURSOR_SHOWING) ||
        !GetCursorPos(&position))
        return;
    /* Wine can report a visible cursor with a null handle until the first
     * real window has selected a class cursor.  A generic arrow is preferable
     * to dropping the cursor from the remote frame altogether. */
    if (cursor.hCursor == NULL)
        cursor.hCursor = LoadCursor(NULL, IDC_ARROW);
    if (cursor.hCursor == NULL)
        return;

    ZeroMemory(&icon, sizeof(icon));
    if (!GetIconInfo(cursor.hCursor, &icon))
        return;

    /* GetIconInfo's mask holds the cursor's actual bitmap dimensions. For a
     * monochrome cursor the AND/XOR masks are stacked vertically. */
    if (icon.hbmMask != NULL &&
        GetObjectW(icon.hbmMask, sizeof(mask), &mask) == sizeof(mask)) {
        shape.x = position.x;
        shape.y = position.y;
        shape.width = mask.bmWidth;
        shape.height = uur_cursor_bitmap_height(mask.bmHeight,
                                                 icon.hbmColor != NULL);
        shape.hotspot_x = (int)icon.xHotspot;
        shape.hotspot_y = (int)icon.yHotspot;
        if (uur_map_cursor_overlay(&source, &destination, &shape, &overlay))
            DrawIconEx(hdc_dest, overlay.x, overlay.y, cursor.hCursor,
                       overlay.width, overlay.height, 0, NULL, DI_NORMAL);
    }
    if (icon.hbmColor != NULL)
        DeleteObject(icon.hbmColor);
    if (icon.hbmMask != NULL)
        DeleteObject(icon.hbmMask);
}

/* Feed the newest desktop frame into the capture destination by drawing
 * our shared-memory frame with StretchDIBits.  Works regardless of the
 * destination bitmap type (DDB or DIB section). */
static BOOL feed_capture_frame_locked(HDC hdc_dest, int x, int y, int width,
                               int height, int source_x, int source_y,
                               int source_width, int source_height)
{
    const unsigned char *header;
    uint32_t fw, fh, fstride;
    uint64_t total;
    int tries;
    int ok = 0;

    if (!ensure_frame_map())
        return FALSE;
    header = (const unsigned char *)frame_map;
    fw = load_u32(header + 8);
    fh = load_u32(header + 12);
    fstride = load_u32(header + 16);
    size_t frame_bytes = (size_t)fstride * fh;
    total = load_u64(header + 24);
    if (fw != frame_width || fh != frame_height || fstride != frame_stride ||
        fw == 0 || fh == 0 || fstride < fw * 4 || total == 0 ||
        frame_bytes / fstride != fh ||
        UURF_HEADER_BYTES + UURF_SLOT_COUNT * frame_slot_bytes > frame_map_size)
        return FALSE;
    if (frame_snapshot_capacity < frame_bytes) {
        unsigned char *fresh = HeapReAlloc(GetProcessHeap(), 0, frame_snapshot,
                                           frame_bytes);
        if (fresh == NULL && frame_snapshot == NULL)
            fresh = HeapAlloc(GetProcessHeap(), 0, frame_bytes);
        if (fresh == NULL)
            return FALSE;
        frame_snapshot = fresh;
        frame_snapshot_capacity = frame_bytes;
    }

    for (tries = 0; tries < 16 && !ok; ++tries) {
        const unsigned char *shm_slot =
            frame_map + uurf_slot_offset(total, (size_t)frame_slot_bytes);
        uint64_t before = load_u64(shm_slot);
        if ((before & 1) == 0 && before / 2 == total) {
            MemoryBarrier();
            memcpy(frame_snapshot, shm_slot + 8, frame_bytes);
            MemoryBarrier();
            uint64_t after = load_u64(shm_slot);
            if (before != after)
                continue;
            BITMAPINFOHEADER bmp_header;
            ZeroMemory(&bmp_header, sizeof(bmp_header));
            bmp_header.biSize = sizeof(BITMAPINFOHEADER);
            bmp_header.biWidth = (LONG)fw;
            bmp_header.biHeight = -(LONG)fh; /* top-down */
            bmp_header.biPlanes = 1;
            bmp_header.biBitCount = 32;
            bmp_header.biCompression = BI_RGB;
            bmp_header.biSizeImage = fstride * fh;

            int screen_width = GetSystemMetrics(SM_CXVIRTUALSCREEN);
            int screen_height = GetSystemMetrics(SM_CYVIRTUALSCREEN);
            if (screen_width < 1)
                screen_width = (int)fw;
            if (screen_height < 1)
                screen_height = (int)fh;
            int sx = (int)((int64_t)source_x * fw / screen_width);
            int sy = (int)((int64_t)source_y * fh / screen_height);
            int sw = (int)((int64_t)source_width * fw / screen_width);
            int sh = (int)((int64_t)source_height * fh / screen_height);
            if (sw < 1)
                sw = (int)fw;
            if (sh < 1)
                sh = (int)fh;
            int result = StretchDIBits(
                hdc_dest, x, y, width, height, sx, sy, sw, sh,
                (const void *)frame_snapshot, (const BITMAPINFO *)&bmp_header,
                DIB_RGB_COLORS, SRCCOPY);
            ok = result != 0 && (DWORD)result != GDI_ERROR;
        } else {
            uint64_t now = load_u64(header + 24);
            total = (now < total) ? now : total - 1;
            if (total == 0)
                return FALSE;
        }
    }
    return ok;
}

static BOOL feed_capture_frame(HDC hdc_dest, int x, int y, int width,
                               int height, int source_x, int source_y,
                               int source_width, int source_height)
{
    AcquireSRWLockExclusive(&frame_lock);
    BOOL result = feed_capture_frame_locked(hdc_dest, x, y, width, height,
        source_x, source_y, source_width, source_height);
    ReleaseSRWLockExclusive(&frame_lock);
    return result;
}

static BOOL try_capture_feed(HDC hdc_dest, int x, int y, DWORD rop,
                             HDC hdc_src, int width, int height, int source_x,
                             int source_y, int source_width, int source_height)
{
    InterlockedIncrement(&capture_calls);
    /* The client's desktop capture is an SRCCOPY blit whose source is the
     * screen DC; anything else falls through untouched. */
    if ((rop & 0x00ffffffu) == SRCCOPY && hdc_src != NULL &&
        source_is_screen_dc(hdc_src)) {
        if (feed_capture_frame(hdc_dest, x, y, width, height, source_x,
                               source_y, source_width, source_height)) {
            InterlockedIncrement(&capture_rendered);
            return TRUE;
        }
    }
    InterlockedIncrement(&capture_fallbacks);
    return FALSE;
}

BOOL WINAPI hook_bit_blt(HDC hdc_dest, int x, int y, int width, int height,
                         HDC hdc_src, int x_src, int y_src, DWORD rop)
{
    BOOL result;
    if (try_capture_feed(hdc_dest, x, y, rop, hdc_src, width, height, x_src,
                         y_src, width, height))
        return TRUE;
    result = original_bit_blt(hdc_dest, x, y, width, height, hdc_src, x_src,
                              y_src, rop);
    if (result && (rop & 0x00ffffffu) == SRCCOPY && hdc_src != NULL &&
        source_is_screen_dc(hdc_src))
        draw_cursor_overlay(hdc_dest, x, y, width, height, x_src, y_src,
                            width, height);
    return result;
}

BOOL WINAPI hook_stretch_blt(HDC hdc_dest, int x, int y, int w, int h,
                             HDC hdc_src, int xs, int ys, int sw, int sh,
                             DWORD rop)
{
    BOOL result;
    if (try_capture_feed(hdc_dest, x, y, rop, hdc_src, w, h, xs, ys, sw, sh))
        return TRUE;
    result = original_stretch_blt(hdc_dest, x, y, w, h, hdc_src, xs, ys, sw,
                                  sh, rop);
    if (result && (rop & 0x00ffffffu) == SRCCOPY && hdc_src != NULL &&
        source_is_screen_dc(hdc_src))
        draw_cursor_overlay(hdc_dest, x, y, w, h, xs, ys, sw, sh);
    return result;
}

/* ---- diagnostics ------------------------------------------------------ */

static void hook_log(const char *text)
{
    char path[MAX_PATH];
    if (GetEnvironmentVariableA("UUR_HOOK_LOG", path, sizeof(path)) == 0 ||
        path[0] == '\0')
        lstrcpynA(path, "C:\\uur-bridge.log", sizeof(path));
    FILE *f = fopen(path, "a");
    if (f == NULL)
        return;
    fprintf(f, "%s\n", text);
    fclose(f);
}

/* ---- wire helpers ---------------------------------------------------- */

static void put_u32(unsigned char *p, uint32_t v)
{
    p[0] = (unsigned char)(v);
    p[1] = (unsigned char)(v >> 8);
    p[2] = (unsigned char)(v >> 16);
    p[3] = (unsigned char)(v >> 24);
}

static void put_u16(unsigned char *p, uint16_t v)
{
    p[0] = (unsigned char)(v);
    p[1] = (unsigned char)(v >> 8);
}

static void put_i32(unsigned char *p, int32_t v)
{
    put_u32(p, (uint32_t)v);
}

static void send_record(uint32_t kind, uint16_t code, uint16_t state,
                        int32_t a, int32_t b)
{
    unsigned char frame[16 + 16];
    put_u32(frame + 0, UUR_MAGIC);
    put_u32(frame + 4, UUR_VERSION);
    put_u32(frame + 8, kind);
    put_u32(frame + 12, 16);
    put_u32(frame + 16, kind);
    put_u16(frame + 20, code);
    put_u16(frame + 22, state);
    put_i32(frame + 24, a);
    put_i32(frame + 28, b);

    int total = 0;
    while (total < (int)sizeof(frame)) {
        int sent = send(hook_socket, (const char *)frame + total,
                        (int)sizeof(frame) - total, 0);
        if (sent <= 0) {
            closesocket(hook_socket);
            hook_socket = INVALID_SOCKET;
            return;
        }
        total += sent;
    }
}

static FARPROC WINAPI hook_get_proc_address(HMODULE module, LPCSTR name)
{
    union {
        FARPROC generic;
        UINT(WINAPI *send_input)(UINT, LPINPUT, int);
        BOOL(WINAPI *bit_blt)(HDC, int, int, int, int, HDC, int, int, DWORD);
        BOOL(WINAPI *stretch_blt)(HDC, int, int, int, int, HDC, int, int,
                                  int, int, DWORD);
        HRESULT(WINAPI *create_dxgi_factory1)(REFIID, void **);
    } replacement;
    FARPROC resolved = original_get_proc_address(module, name);
    if (name == NULL || ((uintptr_t)name >> 16) == 0)
        return resolved;
    if (lstrcmpiA(name, "SendInput") == 0) {
        replacement.send_input = hook_send_input;
        return replacement.generic;
    }
    if (lstrcmpiA(name, "BitBlt") == 0) {
        replacement.bit_blt = hook_bit_blt;
        return replacement.generic;
    }
    if (lstrcmpiA(name, "StretchBlt") == 0) {
        replacement.stretch_blt = hook_stretch_blt;
        return replacement.generic;
    }
    if (lstrcmpiA(name, "CreateDXGIFactory1") == 0) {
        replacement.create_dxgi_factory1 = hook_create_dxgi_factory1;
        return replacement.generic;
    }
    return resolved;
}

/* Minimal ini reader: returns a malloc'd value for `key` or NULL. */
static char *read_ini_value(const char *path, const char *key)
{
    FILE *f = fopen(path, "r");
    char line[512];
    size_t key_len;

    if (f == NULL)
        return NULL;
    key_len = strlen(key);
    while (fgets(line, sizeof(line), f) != NULL) {
        if (strncmp(line, key, key_len) == 0 && line[key_len] == '=') {
            char *end = line + strlen(line);
            char *value = line + key_len + 1;
            char *out;
            size_t len;
            while (end > value &&
                   (end[-1] == '\n' || end[-1] == '\r' || end[-1] == ' '))
                *--end = '\0';
            fclose(f);
            len = (size_t)(end - value);
            out = malloc(len + 1);
            if (out != NULL) {
                memcpy(out, value, len);
                out[len] = '\0';
            }
            return out;
        }
    }
    fclose(f);
    return NULL;
}

static int connect_bridge(void)
{
    char port_text[16] = "47010";
    char token_hex[80] = "";
    char *value;

    /* Config: environment first (fast path for uur-spawned processes),
     * then C:\uur-bridge.ini — the file form survives any spawn path,
     * including the Wine service manager restarting the service without
     * uur's environment. */
    if ((value = getenv("UUR_BRIDGE_PORT")) != NULL && *value)
        lstrcpynA(port_text, value, sizeof(port_text));
    else {
        char *v = read_ini_value("C:\\uur-bridge.ini", "port");
        if (v != NULL) {
            lstrcpynA(port_text, v, sizeof(port_text));
            free(v);
        }
    }
    if ((value = getenv("UUR_BRIDGE_TOKEN")) != NULL && *value)
        lstrcpynA(token_hex, value, sizeof(token_hex));
    else {
        char *v = read_ini_value("C:\\uur-bridge.ini", "token");
        if (v != NULL) {
            lstrcpynA(token_hex, v, sizeof(token_hex));
            free(v);
        }
    }

    WSADATA wsa;
    if (WSAStartup(MAKEWORD(2, 2), &wsa) != 0) {
        hook_log("connect: WSAStartup failed");
        return 0;
    }

    struct addrinfo hints;
    struct addrinfo *result = NULL;
    ZeroMemory(&hints, sizeof(hints));
    hints.ai_family = AF_INET;
    hints.ai_socktype = SOCK_STREAM;
    if (getaddrinfo("127.0.0.1", port_text, &hints, &result) != 0 ||
        result == NULL) {
        char buf[96];
        sprintf(buf, "connect: getaddrinfo failed (port=%s)", port_text);
        hook_log(buf);
        WSACleanup();
        return 0;
    }

    SOCKET sock = socket(result->ai_family, result->ai_socktype,
                         result->ai_protocol);
    if (sock == INVALID_SOCKET) {
        freeaddrinfo(result);
        WSACleanup();
        return 0;
    }
    if (connect(sock, result->ai_addr, (int)result->ai_addrlen) != 0) {
        char buf[96];
        sprintf(buf, "connect: failed wsae=%lu port=%s",
                (unsigned long)WSAGetLastError(), port_text);
        hook_log(buf);
        closesocket(sock);
        freeaddrinfo(result);
        WSACleanup();
        return 0;
    }
    freeaddrinfo(result);

    /* HELLO: header only; the daemon then reads 32 raw token bytes. */
    unsigned char hello[16];
    put_u32(hello + 0, UUR_MAGIC);
    put_u32(hello + 4, UUR_VERSION);
    put_u32(hello + 8, UUR_RECORD_HELLO);
    put_u32(hello + 12, 32);
    if (send(sock, (const char *)hello, sizeof(hello), 0) != sizeof(hello)) {
        closesocket(sock);
        WSACleanup();
        return 0;
    }

    unsigned char token[32];
    ZeroMemory(token, sizeof(token));
    for (int i = 0; i < 32; ++i) {
        unsigned byte = 0;
        char hex[3] = {token_hex[2 * i], token_hex[2 * i + 1], 0};
        if (hex[0] && hex[1]) {
            byte = (unsigned)strtoul(hex, NULL, 16);
        }
        token[i] = (unsigned char)byte;
    }
    if (send(sock, (const char *)token, sizeof(token), 0) != sizeof(token)) {
        closesocket(sock);
        WSACleanup();
        return 0;
    }

    /* Nothing to read until the daemon closes; the connection is
     * fire-and-forget from here on. */
    hook_socket = sock;
    return 1;
}

/* ---- SendInput interception ------------------------------------------ */

UINT WINAPI hook_send_input(UINT count, LPINPUT inputs, int size)
{
    if (inputs == NULL || size < (int)sizeof(INPUT))
        return original_send_input(count, inputs, size);

    LONG busy = InterlockedCompareExchange(&hook_busy, 1, 0);
    if (busy == 0) {
        for (UINT i = 0; i < count; ++i) {
            LPINPUT input = &inputs[i];
            if (input->type == INPUT_KEYBOARD) {
                uint16_t code = input->ki.wVk;
                if (input->ki.dwFlags & KEYEVENTF_EXTENDEDKEY)
                    code |= 0x100;
                send_record(UUR_RECORD_KEYBOARD, code,
                            (input->ki.dwFlags & KEYEVENTF_KEYUP) ? 0 : 1,
                            0, 0);
            } else if (input->type == INPUT_MOUSE) {
                DWORD flags = input->mi.dwFlags;
                if (flags & MOUSEEVENTF_WHEEL) {
                    /* wheel delta rides in the low word of mouseData */
                    send_record(UUR_RECORD_MOUSE, 0xfffe /*wheel*/,
                                0, 0,
                                (int16_t)(input->mi.mouseData & 0xffff));
                } else if (flags & (MOUSEEVENTF_XDOWN | MOUSEEVENTF_XUP)) {
                    /* XBUTTON1/2 map to X11 buttons 8/9 */
                    uint16_t button = (input->mi.mouseData & 0xffff) ==
                                              XBUTTON1
                                          ? 8
                                          : 9;
                    send_record(UUR_RECORD_MOUSE, button,
                                (flags & MOUSEEVENTF_XDOWN) ? 1 : 0, 0, 0);
                } else if (flags & (MOUSEEVENTF_LEFTDOWN | MOUSEEVENTF_LEFTUP |
                                    MOUSEEVENTF_RIGHTDOWN | MOUSEEVENTF_RIGHTUP |
                                    MOUSEEVENTF_MIDDLEDOWN | MOUSEEVENTF_MIDDLEUP)) {
                    uint16_t button = 0;
                    if (flags & (MOUSEEVENTF_LEFTDOWN | MOUSEEVENTF_LEFTUP))
                        button = 1;
                    else if (flags & (MOUSEEVENTF_RIGHTDOWN | MOUSEEVENTF_RIGHTUP))
                        button = 2;
                    else
                        button = 3;
                    int down = (flags & (MOUSEEVENTF_LEFTDOWN |
                                         MOUSEEVENTF_RIGHTDOWN |
                                         MOUSEEVENTF_MIDDLEDOWN)) ? 1 : 0;
                    send_record(UUR_RECORD_MOUSE, button, (uint16_t)down, 0, 0);
                }
                if (flags & MOUSEEVENTF_MOVE) {
                    if (flags & MOUSEEVENTF_ABSOLUTE) {
                        /* Preserve Windows' normalized coordinate space. The
                         * selected Linux backend performs the final mapping. */
                        send_record(UUR_RECORD_MOUSE, 0xffff /*motion*/, 1,
                                    input->mi.dx, input->mi.dy);
                    } else {
                        send_record(UUR_RECORD_MOUSE, 0xffff /*motion*/, 0,
                                    input->mi.dx, input->mi.dy);
                    }
                }
            }
        }
        InterlockedExchange(&hook_busy, 0);
    }

    /* Keep the in-client behaviour identical: UU verifies its calls. */
    return original_send_input(count, inputs, size);
}

/* ---- IAT rewrite ------------------------------------------------------ */

static int patch_module_iat(HMODULE base)
{
    if (base == NULL)
        return 0;

    IMAGE_DOS_HEADER *dos = (IMAGE_DOS_HEADER *)base;
    if (dos->e_magic != IMAGE_DOS_SIGNATURE)
        return 0;
    IMAGE_NT_HEADERS *nt = (IMAGE_NT_HEADERS *)((char *)base + dos->e_lfanew);
    if (nt->Signature != IMAGE_NT_SIGNATURE)
        return 0;

    IMAGE_DATA_DIRECTORY *dir =
        &nt->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT];
    if (dir->VirtualAddress == 0)
        return 0;

    int patched = 0;
    IMAGE_IMPORT_DESCRIPTOR *descriptor = (IMAGE_IMPORT_DESCRIPTOR *)
        ((char *)base + dir->VirtualAddress);
    for (; descriptor->Name != 0; ++descriptor) {
        const char *module_name = (const char *)base + descriptor->Name;
        if (_stricmp(module_name, "USER32.dll") != 0 &&
            _stricmp(module_name, "GDI32.dll") != 0 &&
            _stricmp(module_name, "dxgi.dll") != 0 &&
            _stricmp(module_name, "KERNEL32.dll") != 0 &&
            _stricmp(module_name, "KERNELBASE.dll") != 0)
            continue;

        IMAGE_THUNK_DATA *thunk = (IMAGE_THUNK_DATA *)
            ((char *)base + descriptor->FirstThunk);
        IMAGE_THUNK_DATA *original = thunk;
        if (descriptor->OriginalFirstThunk != 0)
            original = (IMAGE_THUNK_DATA *)
                ((char *)base + descriptor->OriginalFirstThunk);

        for (; original->u1.AddressOfData != 0; ++thunk, ++original) {
            if (original->u1.Ordinal & IMAGE_ORDINAL_FLAG)
                continue;
            IMAGE_IMPORT_BY_NAME *name = (IMAGE_IMPORT_BY_NAME *)
                ((char *)base + original->u1.AddressOfData);
            void *hook_fn = NULL;
            void **original_slot = NULL;

            if (lstrcmpiA(name->Name, "SendInput") == 0) {
                hook_fn = (void *)hook_send_input;
                original_slot = (void **)&original_send_input;
            } else if (lstrcmpiA(name->Name, "BitBlt") == 0) {
                hook_fn = (void *)hook_bit_blt;
                original_slot = (void **)&original_bit_blt;
            } else if (lstrcmpiA(name->Name, "StretchBlt") == 0) {
                hook_fn = (void *)hook_stretch_blt;
                original_slot = (void **)&original_stretch_blt;
            } else if (lstrcmpiA(name->Name, "CreateDXGIFactory1") == 0) {
                hook_fn = (void *)hook_create_dxgi_factory1;
                original_slot = (void **)&original_create_dxgi_factory1;
            } else if (lstrcmpiA(name->Name, "GetProcAddress") == 0) {
                hook_fn = (void *)hook_get_proc_address;
                original_slot = (void **)&original_get_proc_address;
            }
            if (hook_fn == NULL)
                continue;
            if (thunk->u1.Function == (uintptr_t)hook_fn)
                continue;
            *original_slot = (void *)thunk->u1.Function;
            DWORD old_protect = 0;
            if (!VirtualProtect(&thunk->u1.Function, sizeof(thunk->u1.Function),
                                PAGE_READWRITE, &old_protect))
                continue;
            thunk->u1.Function = (uintptr_t)hook_fn;
            VirtualProtect(&thunk->u1.Function, sizeof(thunk->u1.Function),
                           old_protect, &old_protect);
            ++patched;
            InterlockedIncrement(&iat_hooks_installed);
        }
    }
    return patched;
}

static int patch_loaded_modules(void)
{
    int patched = 0;
    HANDLE snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE,
                                               GetCurrentProcessId());
    if (snapshot == INVALID_HANDLE_VALUE)
        return 0;
    MODULEENTRY32W entry;
    entry.dwSize = sizeof(entry);
    if (Module32FirstW(snapshot, &entry)) {
        do {
            patched += patch_module_iat(entry.hModule);
        } while (Module32NextW(snapshot, &entry));
    }
    CloseHandle(snapshot);
    return patched;
}

/* ---- late-module watchdog --------------------------------------------- */

static DWORD WINAPI patch_watchdog(LPVOID parameter)
{
    (void)parameter;
    for (;;) {
        Sleep(2000);
        patch_loaded_modules();
        char status_path[MAX_PATH];
        sprintf(status_path, "C:\\uur-capture-status-%lu.txt",
                (unsigned long)GetCurrentProcessId());
        FILE *f = fopen(status_path, "w");
        if (f != NULL) {
            fprintf(f,
                    "pid=%lu iat=%ld dxgi_f=%ld dxgi_a=%ld dxgi_o=%ld dup=%ld\n\
                     calls=%ld rendered=%ld fallbacks=%ld\n",
                    GetCurrentProcessId(), iat_hooks_installed,
                    dxgi_factories_patched, dxgi_adapters_patched,
                    dxgi_outputs_patched, dxgi_duplicate_calls,
                    capture_calls, capture_rendered, capture_fallbacks);
            fclose(f);
        }
    }
    return 0;
}

static DWORD WINAPI bridge_retry_thread(LPVOID parameter)
{
    (void)parameter;
    for (int attempt = 0; attempt < 120; ++attempt) {
        Sleep(2000);
        if (hook_socket != INVALID_SOCKET)
            return 0;
        if (connect_bridge()) {
            hook_log("retry: bridge connected");
            return 0;
        }
        char buf[96];
        sprintf(buf, "retry %d failed wsae=%lu", attempt,
                (unsigned long)WSAGetLastError());
        hook_log(buf);
    }
    return 0;
}

/* ---- entry ------------------------------------------------------------ */

/* Marker export: the wevtapi shim references this symbol purely so the
 * linker emits the uur-hook.dll dependency that carries the preload chain. */
void uur_hook_version(void) {}

BOOL WINAPI DllMain(HINSTANCE instance, DWORD reason, LPVOID reserved)
{
    (void)instance;
    (void)reserved;
    if (reason != DLL_PROCESS_ATTACH)
        return TRUE;

    DisableThreadLibraryCalls(instance);
    if (connect_bridge()) {
        hook_log("attach: bridge connected");
    } else {
        /* The daemon may simply not be bound yet: the service spawns the
         * server seconds before uur binds the bridge.  Retry in the
         * background instead of staying idle forever. */
        hook_log("attach: bridge unavailable, retrying");
        CreateThread(NULL, 0, bridge_retry_thread, NULL, 0, NULL);
    }
    if (patch_loaded_modules() > 0) {
        hook_log("attach: SendInput/BitBlt imports rewritten");
    } else {
        hook_log("attach: no SendInput import found");
    }

    /* streamer.dll and friends can load after us; keep scanning so late
     * modules get their capture imports rewritten too. */
    HANDLE watchdog = CreateThread(NULL, 0, patch_watchdog, NULL, 0, NULL);
    if (watchdog != NULL)
        CloseHandle(watchdog);
    return TRUE;
}
