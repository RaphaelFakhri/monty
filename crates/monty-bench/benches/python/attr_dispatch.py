xs = []
d = {}
s = 'Monty Benchmark String'
r = 0
for i in range(10_000):
    xs.append(i)
    xs.pop()
    d['k'] = i
    r += d.get('k', 0)
    r += len(s.upper()) + len(s.lower()) + s.count('n') + len(s.split(' ')) + len(s.strip())
r
