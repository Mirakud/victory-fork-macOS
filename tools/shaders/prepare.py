"""Compile Victory's HLSL compute kernels with the DirectX Shader Compiler."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
from tempfile import TemporaryDirectory


ROOT = Path(__file__).resolve().parents[2] / "crates/victory-gpu/src"
SOURCES = ("indirect/clamp.hlsl", "host/card_compare.hlsl", "host/card_compare_parallel.hlsl",
           "memory/init_header.hlsl", "../tests/shaders/raw_fault_pages.hlsl", "draw/room_world.hlsl")


def find_dxc():
    located = shutil.which("dxc")
    if located:
        return located
    sdk = Path(os.environ.get("ProgramFiles(x86)", "C:/Program Files (x86)")) / "Windows Kits/10/bin"
    versions = sorted(sdk.glob("*/x64/dxc.exe"), key=lambda f: tuple(int(n) for n in f.parents[1].name.split(".") if n.isdigit()))
    return str(versions[-1]) if versions else None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dxc", default=os.environ.get("DXC"), help="path to dxc or dxc.exe")
    args = parser.parse_args()
    compiler = args.dxc or find_dxc()
    if not compiler:
        parser.error("Install DXC from Microsoft or set DXC to its executable path.")
    with TemporaryDirectory() as tmp:
        compiled = []
        for relative in SOURCES:
            source = ROOT / relative
            target = Path(tmp) / source.with_suffix(".dxil").name
            profile = "cs_6_6" if source.name == "raw_fault_pages.hlsl" else "cs_6_0"
            entry = "diffuseWorld" if source.name == "room_world.hlsl" else "main"
            subprocess.run([compiler, "-E", entry, "-T", profile, "-Fo", str(target), str(source)], check=True)
            if target.read_bytes()[:4] != b"DXBC":
                raise RuntimeError(f"Invalid DXIL container for {relative}")
            compiled.append((target, source.with_suffix(".dxil")))
        for target, destination in compiled:
            shutil.copyfile(target, destination)
            print(destination.relative_to(ROOT))


if __name__ == "__main__":
    main()
