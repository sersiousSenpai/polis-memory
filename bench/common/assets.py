"""Copy only model files whose bytes match the provider's source-controlled pins."""
import hashlib
from pathlib import Path
import re
import shutil


def prestage(source, destination):
    source = Path(source)
    destination = Path(destination) / "potion-base-8M"
    pins = Path(__file__).resolve().parents[2] / "crates/polis-embed/src/model2vec.rs"
    entries = re.findall(r'name: "([^"]+)",\s*sha256: "([a-f0-9]{64})",\s*bytes: ([\d_]+)', pins.read_text())
    if len(entries) != 3:
        raise RuntimeError("model pin format changed; update prestaging parser")
    verified = {}
    for name, expected, size in entries:
        path = source / "potion-base-8M" / name
        data = path.read_bytes()
        actual = hashlib.sha256(data).hexdigest()
        if actual != expected or len(data) != int(size.replace("_", "")):
            raise RuntimeError(f"embedding asset does not match pinned bytes: {name}")
        verified[name] = actual
    destination.mkdir(parents=True, exist_ok=True)
    for name in verified:
        shutil.copyfile(source / "potion-base-8M" / name, destination / name)
    return verified
