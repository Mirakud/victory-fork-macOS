#!/usr/bin/env python3
"""Assemble un overlay VSP1 depuis des conteneurs capturés et leurs traductions VSH1 locales.

Les clés du cache de traduction suivent le format Victory shader.dxil version 1.
La lecture et la vérification BLAKE3 nécessitent le module Python blake3 ou b3sum.
"""
import argparse
import fcntl
import hashlib
import os
from pathlib import Path
import re
import shutil
import struct
import subprocess
import tempfile

MAGIC = b"VSP1\r\n\x1a\n"
MAX_PACK = 256 << 20
MAX_OVERLAY = 64 << 20
MAX_ENTRIES = 512
FULL_HASH = re.compile("[0-9a-f]{64}")


def hasher():
    try:
        from blake3 import blake3
        return lambda data: blake3(data).digest()
    except ImportError:
        program = shutil.which("b3sum")
        if not program:
            raise ValueError("Installez le module Python blake3 ou la commande b3sum.")
        def digest(data):
            output = subprocess.check_output([program, "--no-names"], input=data)
            return bytes.fromhex(output.decode("ascii").strip())
        return digest


def read_pack(path, digest, *, overlay=False):
    limit = MAX_OVERLAY if overlay else MAX_PACK
    with path.open("rb") as f:
        if not 76 < os.fstat(f.fileno()).st_size <= limit:
            raise ValueError(f"Taille de pack invalide : {path}")
        raw = f.read(limit + 1)
    body = memoryview(raw)[:-32]
    if len(raw) > limit or raw[:8] != MAGIC or digest(body) != raw[-32:]:
        raise ValueError(f"En-tête ou empreinte du pack invalide : {path}")
    build, count = raw[8:40], int.from_bytes(raw[40:44], "little")
    if not 0 < count <= (MAX_ENTRIES if overlay else 8192):
        raise ValueError(f"Nombre de traductions invalide dans {path}")
    entries, pos = {}, 44
    for _ in range(count):
        key = bytes(body[pos:pos + 32])
        size = int.from_bytes(body[pos + 32:pos + 36], "little")
        pos += 36
        if key in entries or size < 4 or pos + size > len(body) or body[pos:pos + 4] != b"VSH1":
            raise ValueError(f"Traduction invalide dans {path}")
        entries[key] = bytes(body[pos:pos + size]) if overlay else None
        pos += size
    if pos != len(body):
        raise ValueError(f"Longueur de pack incohérente dans {path}")
    return build, entries, hashlib.sha256(raw).hexdigest()


def publish(main, inputs, translations, output):
    resolved = output.resolve()
    if any((directory / ".victory-root").is_file() for directory in (resolved, *resolved.parents)):
        raise ValueError("Choisissez un dossier de sortie hors du dépôt Victory.")
    digest = hasher()
    build, base, pack_id = read_pack(main, digest)
    root = output / pack_id
    root.mkdir(parents=True, exist_ok=True)
    with (root / ".lock").open("a+b") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        current = root / "current.vsp"
        entries = {}
        if current.is_symlink():
            target = os.readlink(current)
            match = re.fullmatch(r"versions/([0-9a-f]{64})\.vsp", target)
            if not match or (root / target).is_symlink():
                raise ValueError(f"Cible de version invalide : {current}")
            previous_build, entries, previous_id = read_pack(root / target, digest, overlay=True)
            if previous_build != build or previous_id != match[1] or base.keys() & entries.keys():
                raise ValueError("Overlay existant incompatible avec le pack principal.")
        elif current.exists():
            raise ValueError(f"Un fichier occupe le lien de publication : {current}")

        added = missing = limited = 0
        used = 76 + sum(36 + len(entry) for entry in entries.values())
        for src in sorted(inputs.iterdir()):
            if not FULL_HASH.fullmatch(src.name) or src.is_symlink() or not src.is_file():
                continue
            key = bytes.fromhex(src.name)
            if key in base or key in entries:
                continue
            if not 0 < src.stat().st_size <= 4 << 20:
                raise ValueError(f"Taille de conteneur invalide : {src}")
            guest = src.read_bytes()
            if digest(guest) != key:
                raise ValueError(f"Empreinte du conteneur invalide : {src}")
            name = digest(b"shader.dxil\0" + struct.pack("<I", 1) + build + guest).hex()
            cached = translations / name[:2] / name[2:]
            if not cached.is_file():
                missing += 1
                continue
            size = cached.stat().st_size
            if len(entries) >= MAX_ENTRIES or used + 36 + size > MAX_OVERLAY:
                limited += 1
                continue
            data = cached.read_bytes()
            if len(data) != size or not data.startswith(b"VSH1"):
                raise ValueError(f"Traduction VSH1 invalide : {cached}")
            entries[key] = data
            used += 36 + size
            added += 1
        if added:
            data = bytearray(MAGIC + build + struct.pack("<I", len(entries)))
            for key, blob in sorted(entries.items()):
                data.extend(key + struct.pack("<I", len(blob)))
                data.extend(blob)
            data.extend(digest(data))
            versions = root / "versions"
            versions.mkdir(exist_ok=True)
            version = versions / (hashlib.sha256(data).hexdigest() + ".vsp")
            with tempfile.TemporaryDirectory(prefix=".build-", dir=root) as tmp:
                tmp = Path(tmp)
                packed = tmp / "overlay.vsp"
                packed.write_bytes(data)
                checked_build, checked, _ = read_pack(packed, digest, overlay=True)
                if checked_build != build or checked != entries:
                    raise ValueError("Échec de relecture de l’overlay.")
                packed.chmod(0o444)
                try:
                    os.link(packed, version)
                except FileExistsError:
                    if version.is_symlink() or not version.is_file() or version.read_bytes() != data:
                        raise ValueError(f"Conflit de version : {version}")
                staging = tmp / "current"
                staging.symlink_to(Path("versions") / version.name)
                os.replace(staging, current)
        print(f"Ajouts={added}  traductions absentes={missing}  hors limites={limited}")
        if current.is_symlink():
            print(current)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--shader-pack", required=True, type=Path, help="pack principal du même build de traduction")
    parser.add_argument("--inputs", required=True, type=Path, help="dossier shader-misses d’une session terminée")
    parser.add_argument("--translations", required=True, type=Path, help="cache Victory contenant les blobs VSH1 sous <2 hex>/<62 hex>")
    parser.add_argument("--output", required=True, type=Path, help="dossier local des overlays, regroupés par SHA256 du pack principal")
    args = parser.parse_args()
    try:
        publish(args.shader_pack, args.inputs, args.translations, args.output)
    except (OSError, ValueError, subprocess.CalledProcessError) as exc:
        parser.exit(1, f"{exc}\n")


if __name__ == "__main__":
    main()
