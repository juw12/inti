#!/usr/bin/env python3
"""Can a connection attempt that Python reports as timed out still be accepted by the server?

    python tools/connect_race.py            150 trials
    python tools/connect_race.py 400

tools/vm.py once gave up an attempt to reach QEMU's serial port after one second. This
script plays both sides with plain sockets, without QEMU: the client calls
create_connection(timeout=1), and the server starts to listen between 0.55 s and 0.98 s
later, the way a slowly starting QEMU would. Windows re-sends a connection request every
half second, so the request that can succeed leaves at about 1.0 s, just when the client
gives up. A trial in which the client timed out and the server accepted all the same is
the fault: QEMU would start the machine on a connection nobody holds.

On a system that refuses a connection at once, the client simply fails fast and tries no
second request, and the script reports no such trial.
"""
import json
import random
import socket
import sys
import threading
import time

trials = int(sys.argv[1]) if len(sys.argv) > 1 else 150
counts = {"client ok": 0, "client timed out, server saw nothing": 0,
          "client timed out, server accepted (ghost)": 0, "other": 0}
ghost_delays = []
for trial in range(trials):
    with socket.socket() as finder:
        finder.bind(("127.0.0.1", 0))
        port = finder.getsockname()[1]
    delay = random.uniform(0.55, 0.98)
    outcome = {}

    def client():
        began = time.monotonic()
        try:
            channel = socket.create_connection(("127.0.0.1", port), timeout=1)
            outcome["client"] = "ok"
            time.sleep(0.3)
            channel.close()
        except OSError as error:
            outcome["client"] = type(error).__name__
        outcome["client_s"] = round(time.monotonic() - began, 3)

    thread = threading.Thread(target=client)
    thread.start()
    time.sleep(delay)
    server = socket.socket()
    server.bind(("127.0.0.1", port))
    server.listen(1)
    server.settimeout(1.5)
    try:
        accepted, _ = server.accept()
        accepted.close()
        outcome["server"] = "accepted"
    except OSError:
        outcome["server"] = "nothing"
    server.close()
    thread.join()
    if outcome["client"] == "ok":
        counts["client ok"] += 1
    elif outcome["client"] == "TimeoutError" and outcome["server"] == "accepted":
        counts["client timed out, server accepted (ghost)"] += 1
        ghost_delays.append(round(delay, 3))
    elif outcome["client"] == "TimeoutError":
        counts["client timed out, server saw nothing"] += 1
    else:
        counts["other"] += 1
        print("other:", outcome, flush=True)
print(json.dumps(counts), flush=True)
print("listen delays of the ghosts:", ghost_delays[:20], flush=True)
