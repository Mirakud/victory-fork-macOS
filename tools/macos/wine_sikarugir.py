#!/usr/bin/env python3
"""Adapt cxstart's literal arguments to the isolated Sikarugir Wine loader."""
import argparse
import hashlib
import os
from pathlib import Path
import shlex
import sys


MANAGED_ENGINE = Path.home() / "Library/Application Support/winebottle/Runtimes/sikarugir-10.0_6-gptk4.0b3"
ENGINE = Path(os.environ.get("UNCORK_SIKARUGIR_ENGINE", str(MANAGED_ENGINE))).expanduser().absolute()
PREFIX = Path(os.environ.get("VICTORY_MAC_BOTTLE", "~/Library/Application Support/Victory/bottle-metal4")).expanduser().absolute()
WINE = ENGINE / "bin/wine"
SERVER = WINE.with_name("wineserver")
LIB = ENGINE / "lib"
FRAMEWORK = LIB / "external/D3DMetal.framework/Versions/A/D3DMetal"
APPLE_DLLS = ("d3d10", "d3d11", "d3d12", "d3d12core", "dxgi", "nvapi64", "nvngx-on-metalfx")

def ready():
    if PREFIX.resolve() != PREFIX or ENGINE.resolve() != ENGINE:
        raise ValueError("Le moteur ou la bottle dédiée est redirigé.")
    if not os.access(WINE, os.X_OK) or not os.access(SERVER, os.X_OK):
        raise ValueError("Le chargeur ou son wineserver manque.")
    if "wine sikarugir 10.0 (revision 6)" != (ENGINE / "version").read_text().strip():
        raise ValueError("Révision du moteur inattendue.")
    for line in Path(__file__).with_name("redist-sha256.txt").read_text().splitlines():
        digest, relative = line.split(maxsplit=1)
        if relative.startswith(("external/", "wine/")):
            path = LIB / relative
            with path.open("rb") as f:
                if hashlib.file_digest(f, "sha256").hexdigest() != digest:
                    raise ValueError(f"Composant Apple inattendu : {path}")
    for dll in APPLE_DLLS:
        so = LIB / "wine/x86_64-unix" / (dll + ".so")
        if so.resolve() != (LIB / "external/libd3dshared.dylib").resolve():
            raise ValueError(f"Le lien Unix de {dll} manque.")
    if PREFIX.exists() and (PREFIX / "dosdevices/z:").exists() and (PREFIX / "dosdevices/z:").resolve() != Path("/"):
        raise ValueError("Le disque Z dédié ne pointe pas vers /.")

def build(argv):
    p = argparse.ArgumentParser(allow_abbrev=False)
    p.add_argument("--bottle", required=True, choices=[str(PREFIX)])
    p.add_argument("--no-gui", action="store_true", required=True)
    p.add_argument("--env", default="")
    p.add_argument("--dll", action="append", choices=["d3dcompiler_47=n,b"], default=[])
    p.add_argument("--wait", action="store_true", required=True)
    p.add_argument("exe", type=Path)
    p.add_argument("args", nargs=argparse.REMAINDER)
    a = p.parse_args(argv)
    env = {k: v for k, v in os.environ.items() if not k.startswith(("WINE", "CX_", "VICTORY_", "DYLD_"))}
    settings = dict(WINEPREFIX=str(PREFIX), WINELOADER=str(WINE), WINESERVER=str(SERVER),
                    PATH=str(WINE.parent) + ":" + env.get("PATH", os.defpath),
                    WINEDLLOVERRIDES=";".join([",".join(APPLE_DLLS)+"=b", *a.dll]))
    for entry in shlex.split(a.env):
        key, value = entry.split("=", 1)
        if not key.startswith("VICTORY_"):
            p.error(f"Réglage Victory inattendu : {key}")
        settings[key] = value
    if not a.exe.is_absolute() or not a.exe.is_file():
        p.error(f"Exécutable manquant : {a.exe}")
    return env | settings, [str(WINE), "Z:" + str(a.exe).replace("/", "\\"), *a.args], settings

if __name__ == "__main__":
    preview = sys.argv[1:2] == ["--preview"]
    env, command, settings = build(sys.argv[2:] if preview else sys.argv[1:])
    if preview:
        print(shlex.join(["env", *(f"{k}={v}" for k, v in settings.items()), *command]))
    else:
        try:
            ready()
        except (OSError, ValueError) as error:
            sys.exit(str(error))
        os.execve(WINE, command, env)
