xs = []
d = {'k': 1}
s = 'x'
r = 0
for _ in range(10_000):
    xs.clear()
    xs.reverse()
    r += d.get('k', 0)
    r += 1 if s.startswith('x') else 0
    r += 1 if s.endswith('x') else 0
r
