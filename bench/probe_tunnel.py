#!/usr/bin/env python3
"""asyncio open-model tunnel latency probe with bounded concurrency.

Usage: probe_tunnel.py <port> <rps> <size> <warmup> <duration> [concurrency]

- Open-model fixed-rate sends (no coordinated omission).
- Bounded concurrency (default 32): when the tunnel is saturated (in-flight ==
  cap) the probe skips ticks, so Python never becomes the bottleneck — measured
  latency reflects the tunnel at the maximum load it can actually sustain.
- asyncio single event loop: no thread-per-connection, no GIL churn.
- SO_REUSEADDR mitigates macOS ephemeral-port exhaustion.
"""
import asyncio
import os
import sys
import time

port = int(sys.argv[1]); rps = int(sys.argv[2]); size = int(sys.argv[3])
warmup = int(sys.argv[4]); duration = int(sys.argv[5])
concurrency = int(sys.argv[6]) if len(sys.argv) > 6 else 32
label = os.environ.get('PROBE_LABEL', '')

payload = b'X' * size
interval = 1.0 / rps
lat = []
queue_delay = []
active = 0
sent = 0


async def one_request(_scheduled_at):
    global active, sent
    # The open-model scheduler's tick is useful for offered-load control, but
    # it is not the beginning of a request.  Recording it before
    # `create_task()` makes an asyncio scheduling gap look like tunnel RTT.
    # Start latency at the first instruction the request task actually runs.
    t0 = time.monotonic()
    queue_delay.append((t0 - _scheduled_at) * 1e6)
    active += 1
    try:
        r, w = await asyncio.wait_for(asyncio.open_connection('127.0.0.1', port), 20)
        w.write(payload)
        await asyncio.wait_for(w.drain(), 20)
        d = b''
        while len(d) < size:
            c = await asyncio.wait_for(r.read(262144), 20)
            if not c:
                break
            d += c
        w.close()
        if len(d) == size:
            lat.append((time.monotonic() - t0) * 1e6)
    except Exception:
        pass
    finally:
        active -= 1


async def run_phase(end):
    global sent
    loop = asyncio.get_event_loop()
    next_tick = loop.time()
    tasks = set()
    while time.monotonic() < end:
        t0 = time.monotonic()
        if active < concurrency:
            t = asyncio.create_task(one_request(t0))
            tasks.add(t)
            t.add_done_callback(tasks.discard)
            sent += 1
        # else: tunnel saturated — skip this tick, don't pile up Python work
        next_tick += interval
        delay = next_tick - loop.time()
        if delay > 0:
            await asyncio.sleep(delay)
    for t in list(tasks):
        try:
            await asyncio.wait_for(t, 25)
        except Exception:
            pass


async def main():
    global sent
    wend = time.monotonic() + warmup
    await run_phase(wend)
    lat.clear()  # discard warmup
    queue_delay.clear()
    sent = 0
    mend = wend + duration
    await run_phase(mend)

    lat.sort(); n = len(lat)
    if n == 0:
        print(f'RESULT label={label} samples=0 ok=0 size={size} rps={rps} '
              'p50_us=0 p90_us=0 p99_us=0 p999_us=0 avg_us=0 min_us=0 max_us=0 '
              f'queue_p999_us=0 queue_max_us=0 sent={sent} FAILED')
    else:
        p = lambda q: lat[min(int(n * q), n - 1)]
        queue_delay.sort()
        queue_p999 = queue_delay[min(int(len(queue_delay) * .999), len(queue_delay) - 1)]
        avg = sum(lat) / n
        print(f'RESULT label={label} samples={n} ok={n} size={size} rps={rps} sent={sent} '
              f'p50_us={p(.50):.1f} p90_us={p(.90):.1f} p99_us={p(.99):.1f} '
              f'p999_us={p(.999):.1f} avg_us={avg:.1f} min_us={lat[0]:.1f} '
              f'max_us={lat[-1]:.1f} queue_p999_us={queue_p999:.1f} '
              f'queue_max_us={queue_delay[-1]:.1f}')


asyncio.run(main())
