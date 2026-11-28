#!/usr/bin/env python3
"""Ajoute les traductions positives d’un essai terminé à son overlay ARM64 local."""
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import shlex
import subprocess
import sys
import tempfile

MAX_PACK_BYTES = 256 << 20
MAX_ENTRIES = 512
MAX_BYTES = 64 << 20
HASH = re.compile(r"[0-9a-f]{64}")


def pack_entries(path):
    """Empreinte de chaque traduction du VSP1, par empreinte complète du conteneur."""
    records = {}
    with path.open("rb") as f:
        if not 76 <= os.fstat(f.fileno()).st_size <= MAX_PACK_BYTES:
            raise ValueError(f"Taille de pack incompatible avec Victory dans {path}")
        if f.read(8) != b"VSP1\r\n\x1a\n":
            raise ValueError(f"Format de pack inconnu dans {path}")
        build = f.read(32)
        count = int.from_bytes(f.read(4), "little")
        if not 0 < count <= 8192:
            raise ValueError(f"Nombre d’entrées invalide dans {path}")
        for _ in range(count):
            guest = f.read(32)
            size = int.from_bytes(f.read(4), "little")
            if not 4 <= size <= MAX_PACK_BYTES:
                raise ValueError(f"Taille d’entrée invalide dans {path}")
            blob = f.read(size)
            if len(blob) != size or not blob.startswith(b"VSH1") or guest in records:
                raise ValueError(f"Entrée invalide dans {path}")
            records[guest] = hashlib.sha256(blob).digest()
        if len(f.read(33)) != 32:
            raise ValueError(f"Fin de pack invalide dans {path}")
    return build, records


def captured_inputs(directory):
    if not directory.is_dir():
        return {}
    return {p.name: p for p in directory.iterdir()
            if re.fullmatch(r"[0-9a-f]{64}", p.name) and p.is_file()
            and not p.is_symlink() and 0 < p.stat().st_size <= 4 << 20}


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1 << 20):
            digest.update(chunk)
    return digest.hexdigest()


def sync(path):
    fd = os.open(path, os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def current_version(root, max_bytes=MAX_BYTES):
    link = root / "current.vsp"
    if not link.is_symlink():
        if link.exists():
            raise ValueError(f"Le chemin de publication contient déjà un fichier : {link}")
        return None
    target = os.readlink(link)
    match = re.fullmatch(r"versions/([0-9a-f]{64})\.vsp", target)
    if not match:
        raise ValueError(f"Cible d’overlay inattendue : {target}")
    path = root / target
    if (path.is_symlink() or not path.is_file() or not 76 < path.stat().st_size <= max_bytes
            or sha256(path) != match[1]):
        raise ValueError(f"Version publiée absente ou modifiée : {path}")
    return path


def native(preparer, cache, env, mode, ids, pending, pack, *, refused=None):
    request = pending / "ids.txt"
    request.write_text("".join(k + "\n" for k in sorted(ids)), encoding="ascii")
    result = subprocess.run(
        [preparer, mode, request, pending / "inputs",
         cache / "cache-readonly-final", *([pack] if mode != "warm" else [])],
        env=env, check=True, capture_output=True, text=True,
    )
    if mode == "verify":
        return {}
    sizes = {}
    for line in result.stdout.splitlines():
        fields = line.split()
        if len(fields) < 2 or fields[0] not in ids or fields[0] in sizes:
            raise ValueError(f"Réponse native inattendue : {line}")
        key, status = fields[:2]
        if status in ("MADE", "HIT") and len(fields) == 5:
            sizes[key] = int(fields[3])
            if sizes[key] < 4:
                raise ValueError(f"Taille de traduction native invalide : {line}")
        elif status == "FAILED":
            if refused is not None:
                reason = line.split(" ", 2)[-1]
                if not reason.startswith("converter shader.dxil v1: "):
                    raise ValueError(f"Échec du cache natif : {line}")
                refused[key] = reason
            sizes[key] = None
        else:
            raise ValueError(f"Conteneur non traité : {line}")
    if sizes.keys() != set(ids):
        raise ValueError("Inventaire natif incomplet.")
    return sizes


def publish_version(root, pending, pack):
    versions = root / "versions"
    versions.mkdir(exist_ok=True)
    version_hash = sha256(pack)
    version = versions / (version_hash + ".vsp")
    pack.chmod(0o444)
    sync(pack)
    try:
        os.link(pack, version)
    except FileExistsError:
        if version.is_symlink() or not version.is_file() or sha256(version) != version_hash:
            raise ValueError(f"Conflit sur la version immuable : {version}")
    sync(versions)
    temporary_link = pending / "next"
    temporary_link.symlink_to(Path("versions") / version.name)
    os.replace(temporary_link, root / "current.vsp")
    sync(root)


def fixed_root(cache, primary_sha256, fixed_sha256):
    if not all(isinstance(v, str) and HASH.fullmatch(v) for v in (primary_sha256, fixed_sha256)):
        raise ValueError("Empreintes du pack principal ou de l’overlay fixe invalides.")
    return cache / "fixed-overlays" / primary_sha256 / fixed_sha256


def select_fixed_overlay(fixed, primary_sha256, fixed_sha256, cache):
    """Return the immutable learned version or the original fixed pack. No writes."""
    root = fixed_root(cache, primary_sha256, fixed_sha256)
    try:
        version = current_version(root, MAX_PACK_BYTES)
    except (OSError, ValueError) as exc:
        print(f"Version apprise illisible, utilisation du complément fixe. {exc}", file=sys.stderr)
        version = None
    return version or fixed.resolve(strict=True)


def read_refusals(path):
    if not path.exists():
        return {}
    refused = json.loads(path.read_text(encoding="utf8"))
    if not isinstance(refused, dict) or not all(
        HASH.fullmatch(k) and isinstance(v, str) and v.startswith("converter shader.dxil v1: ")
        for k, v in refused.items()
    ):
        raise ValueError(f"Inventaire de refus invalide dans {path}")
    return refused


def save_refusals(root, target, refused):
    with tempfile.TemporaryDirectory(prefix=".refus-", dir=root) as folder:
        staged = Path(folder) / "refus.json"
        staged.write_text(json.dumps(refused, sort_keys=True, ensure_ascii=False) + "\n", encoding="utf8")
        sync(staged)
        os.replace(staged, target)
        sync(root)


def extend_fixed(run, fixed, preparer, cache):
    """Append only new verified translations to a fixed, release-specific seed."""
    record = json.loads((run / "launch.json").read_text())
    if record.get("mode") != "run" or record.get("dry_run") or type(record.get("exit_code")) is not int:
        raise ValueError("Un essai réel terminé avec son code de sortie est requis.")
    captured = captured_inputs(run / "shader-misses")
    if not captured:
        return "Aucun nouveau conteneur."
    fixed = fixed.resolve(strict=True)
    primary = Path(record["readonly_shader_pack"]).resolve(strict=True)
    primary_sha = record["readonly_shader_pack_sha256"]
    if sha256(primary) != primary_sha:
        raise ValueError("Le pack principal de la session a changé.")
    fixed_sha = sha256(fixed)
    if record.get("fixed_shader_overlay_sha256", fixed_sha) != fixed_sha:
        raise ValueError("L’overlay fixe de la session a changé.")
    root = fixed_root(cache, primary_sha, fixed_sha)
    observed = record.get("readonly_shader_overlay_sha256")
    if not isinstance(observed, str) or not HASH.fullmatch(observed):
        raise ValueError("La session ne désigne pas son overlay de shaders.")
    if observed != fixed_sha:
        version = root / "versions" / (observed + ".vsp")
        if version.is_symlink() or not version.is_file() or sha256(version) != observed:
            raise ValueError("L’overlay utilisé par la session ne provient pas de ce couple de packs.")
    build, main = pack_entries(primary)
    fixed_build, base = pack_entries(fixed)
    if fixed_build != build:
        raise ValueError("Le principal et le complément fixe utilisent des traducteurs différents.")
    fresh = {k: p for k, p in captured.items() if bytes.fromhex(k) not in main and bytes.fromhex(k) not in base}
    if not fresh:
        return "Les deux packs fixes couvrent déjà les captures."

    root.mkdir(parents=True, exist_ok=True)
    with (root / ".lock").open("a+b") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        prior = current_version(root, MAX_PACK_BYTES) or fixed
        old = base
        if prior != fixed:
            prior_build, old = pack_entries(prior)
            if prior_build != build or not base.items() <= old.items():
                raise ValueError("Une traduction prioritaire a changé dans la version apprise.")
        remaining = {k: p for k, p in fresh.items() if bytes.fromhex(k) not in old}
        if not remaining:
            return "La version apprise couvre déjà les captures."

        helper_sha = sha256(preparer)
        refusal_path = root / ("refus-" + helper_sha + ".json")
        refused = read_refusals(refusal_path)
        room = min(8192 - len(old), MAX_ENTRIES - (len(old) - len(base)))
        budget = min(MAX_PACK_BYTES, fixed.stat().st_size + MAX_BYTES) - prior.stat().st_size
        if room <= 0 or budget < 40:
            return "Capacité de l’extension atteinte."
        candidates = [k for k in sorted(remaining) if k not in refused]
        if not candidates:
            return "Les conteneurs restants ont déjà été refusés par ce traducteur."

        capabilities = subprocess.run([preparer, "--help"], check=True, capture_output=True, text=True)
        if "compose FIXED CURRENT DELTA OUTPUT" not in capabilities.stdout:
            raise ValueError("Cet outil ARM ne sait pas préserver les traductions du complément fixe.")
        command = record["command"]
        settings = shlex.split(command[command.index("--env") + 1])
        env = {k: v for k, v in os.environ.items() if not k.startswith("VICTORY_") or k == "VICTORY_SHADER_JOBS"}
        env.update(x.split("=", 1) for x in settings if x.startswith("VICTORY_"))
        for key in ("VICTORY_READONLY_SHADER_OVERLAY", "VICTORY_SHADER_MISSES_DIR", "VICTORY_PREPARE_REFUSALS_FILE"):
            env.pop(key, None)
        env["VICTORY_SHADER_REFUSAL_FILE"] = str(root / ("refus-" + helper_sha + ".vref"))
        env["VICTORY_PREPARE_VERIFY_IDS"] = "1"
        with tempfile.TemporaryDirectory(prefix=".build-", dir=root) as folder:
            tmp = Path(folder)
            inputs = tmp / "inputs"
            inputs.mkdir()
            chosen = []
            source_bytes = 0
            for k in candidates:
                size = remaining[k].stat().st_size
                if len(chosen) >= MAX_ENTRIES:
                    break
                if source_bytes + size <= MAX_BYTES:
                    shutil.copyfile(remaining[k], inputs / k)
                    source_bytes += size
                    chosen.append(k)
            if not chosen:
                return "Capacité de lecture des nouveaux conteneurs atteinte."
            negative = {}
            delta = tmp / "ajouts.vsp"
            sizes = native(preparer, cache, env, "warm", set(chosen), tmp, delta, refused=negative)
            added = []
            for k, size in sorted(sizes.items()):
                if size is not None and len(added) < room and size + 36 <= budget:
                    added.append(k)
                    budget -= size + 36
            if added:
                wanted = set(added)
                built = native(preparer, cache, env, "pack", wanted, tmp, delta, refused={})
                delta_build, new = pack_entries(delta)
                if delta_build != build or [k.hex() for k in new] != added or any(built[k] != sizes[k] for k in added):
                    raise ValueError("L’identité de traduction ou les entrées du lot divergent.")
                pack = tmp / "candidate.vsp"
                subprocess.run([preparer, "compose", fixed, prior, delta, pack], check=True,
                               capture_output=True, text=True, env=env)
                combined_build, combined = pack_entries(pack)
                if combined_build != build or combined != (old | new):
                    raise ValueError("La fusion des anciennes traductions et des nouvelles a divergé.")
                native(preparer, cache, env, "verify", wanted, tmp, pack)
                publish_version(root, tmp, pack)
            if negative:
                save_refusals(root, refusal_path, refused | negative)
            return (f"Overlay prioritaire {len(base)}, appris {len(old) - len(base) + len(added)}, "
                    f"ajout {len(added)}, refus {len(negative)}.")
