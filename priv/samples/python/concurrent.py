"""Demonstrates threading inside the sandbox.

Requires the `concurrency` capability (default for python-base in the
catalog). Without it, pthread `clone` would be killed by seccomp.
"""

import threading
import time


def worker(name: str, delay: float) -> None:
    time.sleep(delay)
    print(f"[{name}] done after {delay:.2f}s")


threads = [
    threading.Thread(target=worker, args=(f"t{i}", 0.05 * (i + 1)))
    for i in range(5)
]
for t in threads:
    t.start()
for t in threads:
    t.join()
print("all threads joined")
