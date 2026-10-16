import gc

gc.disable()
for _ in range(50_000):
    a = []
    a.append(a)
gc.collect()
0
