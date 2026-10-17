/*
 * winlogon.exe — session presence marker.
 *
 * GameViewerService enumerates processes for a winlogon session token
 * source before it initialises; under plain Wine none exists and the
 * service never leaves its initial state (the launcher waits at the
 * splash forever). It also reports the client's Windows process presence
 * over its inherited pipe, avoiding a new Wine/tasklist process per poll.
 */

#include <windows.h>
#include <tlhelp32.h>

static char client_present(const WCHAR *image)
{
    PROCESSENTRY32W entry = {0};
    HANDLE snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
    if (snapshot == INVALID_HANDLE_VALUE)
        return '?';
    entry.dwSize = sizeof(entry);
    if (!Process32FirstW(snapshot, &entry)) {
        CloseHandle(snapshot);
        return '?';
    }
    do {
        if (!lstrcmpiW(entry.szExeFile, image)) {
            CloseHandle(snapshot);
            return '1';
        }
    } while (Process32NextW(snapshot, &entry));
    DWORD error = GetLastError();
    CloseHandle(snapshot);
    return error == ERROR_NO_MORE_FILES ? '0' : '?';
}

int WINAPI WinMain(HINSTANCE instance, HINSTANCE previous, LPSTR command_line,
                   int show_command)
{
    (void)instance;
    (void)previous;
    (void)command_line;
    (void)show_command;
    WCHAR image[MAX_PATH];
    DWORD length = GetEnvironmentVariableW(L"UUR_MONITOR_CLIENT", image,
                                           MAX_PATH);
    if (!length) {
        Sleep(INFINITE);
        return 0;
    }
    if (length >= MAX_PATH)
        return 1;
    HANDLE output = GetStdHandle(STD_OUTPUT_HANDLE);
    for (;;) {
        char state = client_present(image);
        DWORD written;
        if (!WriteFile(output, &state, 1, &written, NULL) || written != 1)
            return 1;
        Sleep(500);
    }
}
