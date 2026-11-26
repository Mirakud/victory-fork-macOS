"""Lance Victory avec D3DMetal 4 et le moniteur mémoire sur un dossier de jeu local préparé."""
import argparse
import json
import os
from pathlib import Path
import shlex
import signal
import shutil
import subprocess
import sys
from tempfile import mkdtemp

TOOLS = Path(__file__).resolve().parent
PROFILE = {
    "VICTORY_LATE_WAIT_LISTS": "16", "VICTORY_IDLE_HOST_LISTS": "16",
    "VICTORY_WINDOW_KB": "4", "VICTORY_WINDOW_TAILS": "1", "VICTORY_WINDOW_REUSE": "1",
    "VICTORY_PIPELINE_WORKERS": "2", "VICTORY_PIPELINE_INTERVAL_MS": "0",
    "VICTORY_INDIRECT_MESH_GROUP_LIMIT": "1024", "VICTORY_CARD_BUFFERS_MB": "256",
    "VICTORY_TEXTURE_BUDGET_MB": "96", "VICTORY_TEXTURE_DROP_MIPS": "1",
    "VICTORY_LAZY_HOST_LISTS": "1", "VICTORY_STORAGE_POLL_PAUSE": "1",
    "VICTORY_REPACK_METAL_MESH_VARYINGS": "1", "VICTORY_MESH_REPACK_KEEP_CLIPS": "1",
    "VICTORY_SHADER_DISK_CACHE": "0", "VICTORY_WIDE_REFRESH": "1", "VICTORY_SPARSE_READ_HINT": "1",
    "VICTORY_COPY_CHANGED": "1", "VICTORY_PORTABLE_VIEW_HEAP": "1",
    "VICTORY_LOG": "warn,victory_gpu::host=info,victory_gpu::shadow=info",
}


def absolute(path):
    return Path(path).expanduser().resolve()


def drive_z(path):
    return "Z:" + path.as_posix()


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for name in ("victory", "title", "engine", "bottle", "output"):
        p.add_argument("--" + name, type=absolute, required=True)
    p.add_argument("--data", type=absolute, help="dossier de données Victory à copier pour cet essai")
    p.add_argument("--shader-pack", type=absolute, help="pack privé compatible avec le binaire utilisé")
    p.add_argument("--pipeline-manifest", type=absolute, help="liste privée de clés à préparer")
    p.add_argument("--perf-hud", action="store_true", help="afficher les FPS mesurés et la mémoire de Victory au-dessus du jeu")
    p.add_argument("--preview", action="store_true", help="afficher la commande sans créer de dossier ni lancer Wine")
    args = p.parse_args()
    for path in (args.victory, args.title, args.shader_pack, args.pipeline_manifest):
        if path and not path.is_file():
            p.error(f"fichier manquant : {path}")
    for path in (args.bottle, args.engine, args.data):
        if path and not path.is_dir():
            p.error(f"dossier manquant : {path}")
    if args.data and (args.output == args.data or args.data in args.output.parents):
        p.error("le dossier de sortie doit être hors des données à copier")
    if args.preview:
        run = args.output / "session-preview"
    else:
        args.output.mkdir(parents=True, exist_ok=True)
        run = Path(mkdtemp(prefix="session-", dir=args.output))
        if args.data:
            shutil.copytree(args.data, run / "data")
        for folder in ("data", "cache", "captures"):
            (run / folder).mkdir(exist_ok=True)
    config = dict(PROFILE, VICTORY_GAME_DIR=drive_z(args.title.parent), VICTORY_DATA_DIR=drive_z(run/"data"),
                  VICTORY_CACHE_DIR=drive_z(run/"cache"), VICTORY_CAPTURE=drive_z(run/"captures"))
    if args.shader_pack:
        config["VICTORY_READONLY_SHADER_PACK"] = drive_z(args.shader_pack)
    if args.pipeline_manifest:
        config["VICTORY_PREWARM_PIPELINE_MANIFEST"] = drive_z(args.pipeline_manifest)
    child = [sys.executable, str(TOOLS/"wine_sikarugir.py"), "--bottle", str(args.bottle),
             "--no-gui", "--env", shlex.join([key+"="+value for key, value in config.items()]),
             "--wait", str(args.victory), "run", drive_z(args.title), "--execute", "--console", "series-s"]
    command = [sys.executable, str(TOOLS/"monitor_xcl_run.py"), "--max-wired-mib", "9216",
               "--report-peak-used-percent", "95", "--poll-interval", "0.05",
               "--telemetry-path", str(run/"telemetry.jsonl"), "--", *child]
    if args.preview:
        print(shlex.join(command))
        if args.perf_hud:
            print("Encart natif demandé (FPS, mémoire de Victory, mirakud).")
        return 0
    if args.perf_hud:
        from victory_perf_feed import PerformanceHUD, build_hud
        hud_binary = build_hud(TOOLS / "victory_perf_hud.swift")
    env = dict(os.environ, UNCORK_SIKARUGIR_ENGINE=str(args.engine), VICTORY_MAC_BOTTLE=str(args.bottle))
    print(run, flush=True)
    with (run / "console.txt").open("wb") as console:
        monitor = subprocess.Popen(command, env=env, stdout=console, stderr=subprocess.STDOUT, start_new_session=True)
        hud = None

        def stop_monitor(_signum, _frame):
            monitor.send_signal(signal.SIGINT)

        handlers = {sig: signal.signal(sig, stop_monitor) for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)}
        try:
            if args.perf_hud:
                try:
                    hud = PerformanceHUD(hud_binary, run, args.victory, monitor.pid)
                except OSError as error:
                    print(f"Encart indisponible : {error}", file=sys.stderr)
            rc = monitor.wait()
        finally:
            if hud is not None:
                hud.close()
            for sig, handler in handlers.items():
                signal.signal(sig, handler)
    (run / "launch.json").write_text(json.dumps({"mode": "run", "exit_code": rc, "perf_hud": args.perf_hud, "command": command}, indent=2))
    return rc


if __name__ == "__main__":
    raise SystemExit(main())
