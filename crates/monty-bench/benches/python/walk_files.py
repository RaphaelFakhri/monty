import os

count = 0
for _, _, files in os.walk(DATA):
    count += len(files)
count
