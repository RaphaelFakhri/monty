import json

obj = json.loads(DATA)
r = 0
for _ in range(1_000):
    r = len(json.dumps(obj))
r
