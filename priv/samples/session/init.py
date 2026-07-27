# Bootstrap cell for a session. The session runner keeps state across cells,
# so `state` persists in subsequent `sessions/:id/execute` calls.
state = {"counter": 0, "history": []}
print("session ready")
