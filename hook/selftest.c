/*
 * selftest — end-to-end probe without NetEase software.
 *
 * Statically imports wevtapi!EvtOpenPublisherMetadata so the Windows
 * loader resolves the preload chain (wevtapi.dll -> uur-hook.dll) at
 * process start, and USER32!SendInput so the hook has something to
 * rewrite.  Then it issues a few absolute pointer motions and one key
 * press/release pair.  The uur host bridge should log the connection and
 * inject the motions onto the real desktop; `uur pointer` before/after
 * observes the effect.
 */

#include <windows.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

#include "frame-protocol.h"

/* Static import: forces wevtapi.dll (the shim) to load at startup. */
void *__stdcall EvtOpenPublisherMetadata(void *session, void *publisher_id,
                                         void *log_file_path,
                                         unsigned long locale,
                                         unsigned long flags);

static void move_absolute(int x, int y)
{
    INPUT input;
    ZeroMemory(&input, sizeof(input));
    input.type = INPUT_MOUSE;
    input.mi.dwFlags = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE;
    input.mi.dx = (DWORD)((int64_t)x * 65535 / 1920);
    input.mi.dy = (DWORD)((int64_t)y * 65535 / 1080);
    SendInput(1, &input, sizeof(input));
}

static void put_u32(unsigned char *target, uint32_t value)
{
    target[0] = (unsigned char)value;
    target[1] = (unsigned char)(value >> 8);
    target[2] = (unsigned char)(value >> 16);
    target[3] = (unsigned char)(value >> 24);
}

static void put_u64(unsigned char *target, uint64_t value)
{
    put_u32(target, (uint32_t)value);
    put_u32(target + 4, (uint32_t)(value >> 32));
}

static int capture_round_trip(void)
{
    static const char frame_path[] = "C:\\uur-selftest-frames.bin";
    const uint32_t width = 2;
    const uint32_t height = 2;
    const uint32_t stride = width * 4;
    const size_t slot_bytes = 8 + stride * height;
    const size_t total_bytes = UURF_HEADER_BYTES + UURF_SLOT_COUNT * slot_bytes;
    unsigned char *transport = calloc(1, total_bytes);
    DWORD written = 0;
    HANDLE file;
    HDC screen = NULL;
    HDC destination = NULL;
    HBITMAP bitmap = NULL;
    HGDIOBJ previous = NULL;
    unsigned char *pixels = NULL;
    int passed = 0;

    if (transport == NULL)
        return 0;
    put_u32(transport, UURF_MAGIC);
    put_u32(transport + 4, UURF_VERSION);
    put_u32(transport + 8, width);
    put_u32(transport + 12, height);
    put_u32(transport + 16, stride);
    put_u32(transport + 20, 1);
    put_u64(transport + 24, 1);
    put_u32(transport + 36, 1);
    put_u32(transport + UURF_BASE_HEADER_BYTES + 8, width);
    put_u32(transport + UURF_BASE_HEADER_BYTES + 12, height);
    put_u32(transport + UURF_BASE_HEADER_BYTES + 24, width);
    put_u32(transport + UURF_BASE_HEADER_BYTES + 28, height);
    unsigned char *slot = transport + uurf_slot_offset(1, slot_bytes);
    put_u64(slot, 2);
    for (size_t offset = 8; offset < slot_bytes; offset += 4) {
        slot[offset + 0] = 0x00;
        slot[offset + 1] = 0x00;
        slot[offset + 2] = 0xff;
        slot[offset + 3] = 0x00;
    }

    file = CreateFileA(frame_path, GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE,
                       NULL, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
    if (file == INVALID_HANDLE_VALUE)
        goto cleanup;
    if (!WriteFile(file, transport, (DWORD)total_bytes, &written, NULL) ||
        written != total_bytes) {
        CloseHandle(file);
        goto cleanup;
    }
    CloseHandle(file);
    if (!WritePrivateProfileStringA("bridge", "frame_path", frame_path,
                                    "C:\\uur-bridge.ini"))
        goto cleanup;

    BITMAPINFO info;
    ZeroMemory(&info, sizeof(info));
    info.bmiHeader.biSize = sizeof(BITMAPINFOHEADER);
    info.bmiHeader.biWidth = (LONG)width;
    info.bmiHeader.biHeight = -(LONG)height;
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 32;
    info.bmiHeader.biCompression = BI_RGB;

    screen = GetDC(NULL);
    destination = CreateCompatibleDC(screen);
    bitmap = CreateDIBSection(destination, &info, DIB_RGB_COLORS,
                              (void **)&pixels, NULL, 0);
    if (screen == NULL || destination == NULL || bitmap == NULL || pixels == NULL)
        goto cleanup;
    previous = SelectObject(destination, bitmap);
    if (!BitBlt(destination, 0, 0, width, height, screen, 0, 0, SRCCOPY))
        goto cleanup;
    passed = pixels[0] == 0x00 && pixels[1] == 0x00 && pixels[2] == 0xff;

cleanup:
    if (previous != NULL)
        SelectObject(destination, previous);
    if (bitmap != NULL)
        DeleteObject(bitmap);
    if (destination != NULL)
        DeleteDC(destination);
    if (screen != NULL)
        ReleaseDC(NULL, screen);
    DeleteFileA(frame_path);
    free(transport);
    return passed;
}

int main(void)
{
    (void)EvtOpenPublisherMetadata(NULL, NULL, NULL, 0, 0);

    if (!capture_round_trip()) {
        fprintf(stderr, "capture adapter round-trip failed\n");
        return 10;
    }

    move_absolute(200, 200);
    Sleep(400);
    move_absolute(700, 500);
    Sleep(400);
    move_absolute(960, 540);
    Sleep(400);

    INPUT key;
    ZeroMemory(&key, sizeof(key));
    key.type = INPUT_KEYBOARD;
    key.ki.wVk = 0x41; /* 'A' */
    SendInput(1, &key, sizeof(key));
    key.ki.dwFlags = KEYEVENTF_KEYUP;
    SendInput(1, &key, sizeof(key));


    Sleep(200);
    printf("hook capture and input smoke test passed\n");
    return 0;
}
