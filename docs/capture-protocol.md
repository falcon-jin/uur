# Capture frame protocol

How frames travel from the compositor (via PipeWire) into the client's
capture calls.  Two producers/consumers, one shared file, no locks.

```text
xdg-desktop-portal ScreenCast
        │ (portal-consented session; per-monitor)
        ▼
uur-pw-capture (C) ── PipeWire stream, BGRx, SHM buffers
        │ writes slots, seqlock per slot
        ▼
$XDG_RUNTIME_DIR/uur/frames.v1  (mode 0600, 64-byte header + 3 slots)
        │ mapped read-only by the PE hook through its configured Wine Z: path
        ▼
uur-hook.dll ── intercepts static/dynamic gdi32!BitBlt/StretchBlt calls;
                scales the newest stable snapshot into the destination DIB
```

## Shared file layout

All integers little-endian.  Total size = 64 + 3 × (8 + stride × height).

| offset | type | field |
| --- | --- | --- |
| 0 | u32 | magic `0x46525555` ("UURF") |
| 4 | u32 | protocol version = 1 |
| 8 | u32 | frame width (px) |
| 12 | u32 | frame height (px) |
| 16 | u32 | stride (bytes per row) |
| 20 | u32 | format: 1 = BGRx (32 bits per pixel) |
| 24 | u64 | total frames written (monotonic counter) |
| 32..64 | — | reserved, zero |

Slot *i* (i = 0, 1, 2) at offset `64 + i × (8 + stride × height)`:

| offset | type | field |
| --- | --- | --- |
| 0 | u64 | sequence: `2×n − 1` while slot n is being written, `2×n` when slot n is stable |
| 8 | bytes | frame, `stride × height`, rows top-to-bottom, pixels BGRx |

Writer picks slots round-robin (`n mod 3`), bumps the counter, publishes
the odd sequence (release), copies the frame, then publishes the even
sequence (release).  The reader is a classic seqlock read: load the
sequence, copy, re-load; retry while the value changed or is odd.  No
mutexes; a torn read is detected, never observed.

The transport path is created per user and passed to the Wine adapter through
the managed prefix configuration. It is never a global shared-memory name.

## Geometry and session changes

Each transport inode has immutable geometry. A negotiated size change creates
a new mode-0600 file, initializes its header and slots, then atomically replaces
the transport path. The previous mapping remains valid for existing readers;
the producer never truncates it. The Wine adapter checks file identity at least
every 250 ms and remaps replacements while serializing its capture readers.

The PipeWire consumer accepts explicitly linear DMA-BUFs and retains a separate
shared-memory format offer. DMA-BUF reads use the source stride, plane offsets,
fd extent, readiness fence and CPU-access synchronization. Implicit modifiers
require a graphics importer and are not advertised by this CPU consumer.

A stream error, lost connection or ten seconds in PAUSED after streaming makes
the helper exit with status 75. The ScreenCast supervisor retries up to three
times using the saved restore token, with a one-second backoff. Thirty seconds
of healthy capture resets the retry budget. An unchanged desktop in STREAMING
does not trigger this watchdog. Recovery keeps the saved token and performs no
token-less fallback or switch to direct capture. The portal controls whether
restoring that choice requires confirmation. A portal Session.Closed signal
without a matching Mutter layout change ends sharing; the user starts a new
session explicitly. The supervisor subscribes to the raw signal on the session's
own connection, accepting both dictionary and empty signal bodies.

When Mutter's DisplayConfig service is already present, the supervisor also
subscribes to MonitorsChanged and proactively restores capture after a layout
change. It coalesces signals during the reconnect delay. Mutter may close the
old session before forwarding MonitorsChanged, so a Closed signal has a 250 ms
window to correlate that transaction. Closed without a matching change stops
capture, and process stop requests always take priority. This optional listener
never starts a GNOME service on other desktops.
Successful layout-driven restarts do not consume the failure retry budget.
Only the initial session can make a token-less interactive selection; later
restores keep that boundary even when the failure counter has been reset.
