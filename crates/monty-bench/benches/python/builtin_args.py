r = 0
for i in range(10_000):
    s = 'ab'.replace('a', 'b')
    t = 'x'.encode('utf-8', 'strict')
    u = sorted([3, 1, 2], reverse=False)
    r += len(s) + len(t) + len(u)
r
