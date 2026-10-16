from pathlib import Path

len(list(Path(DATA).glob('**/*.rs')))
