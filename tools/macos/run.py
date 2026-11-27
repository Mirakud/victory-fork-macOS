"""Lance Victory avec D3DMetal 4 et le moniteur mémoire sur un dossier de jeu local préparé."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import signal
import shutil
import subprocess
import sys
from tempfile import mkdtemp
import xml.etree.ElementTree as ET

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


# Profil expérimental de la cinématique Series S, activé à la demande.
INTRO = {
    "VICTORY_TEXTURE_NO_WAIT": "1", "VICTORY_FAST_MIRROR_COMPARE": "1",
    "VICTORY_SMALL_SPARSE_READ": "1", "VICTORY_SPARSE_READ_RUNS": "1",
    "VICTORY_STORAGE_VIEW_CACHE": "1", "VICTORY_STORAGE_QUEUE_STATS": "1",
    "VICTORY_RELAY_STALE_WRITES": "1", "VICTORY_DECLINE_RECORDED_NOOPS": "1",
    "VICTORY_TRACE_STALLED_MESH": "1", "VICTORY_MAP_ALIAS_STATS": "1",
    "VICTORY_TITLE_POOL_WATCH": "1", "VICTORY_TITLE_SRL_WATCH": "1",
    "VICTORY_CARGEN_RESERVE": "1", "VICTORY_MAP_POOL_RESERVE": "building",
    "VICTORY_CAPTURE_WORLD": "only", "VICTORY_CAPTURE_WORLD_INTERVAL": "1",
    "VICTORY_MEMORY_WATCH": "1",
    "VICTORY_LOG": "warn,victory_gpu::memory_watch=info,victory_gpu::host=info,"
                   "victory_gpu::shadow=info,victory_gpu::title_pool=info,"
                   "victory_hle::modules::dstorage=info,victory_hle::modules::xmem=info",
}


def sha256(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def pipeline_key(value):
    if not re.fullmatch("[0-9a-f]{32}", value):
        raise argparse.ArgumentTypeError("clé attendue sur 32 chiffres hexadécimaux minuscules")
    return value


def intro_settings(title, data):
    """Prépare les deux changements sur une copie tout en conservant le reste du XML."""
    title_id = ET.parse(title.parent / "MicrosoftGame.Config").getroot().findtext("TitleId", "").strip()
    if not re.fullmatch("[0-9A-Fa-f]{8}", title_id):
        raise ValueError("TitleId absent du fichier MicrosoftGame.Config fourni")
    relative = Path(title_id.upper()) / "storage/settings/settings.xml"
    raw = (data / relative).read_bytes()
    for tag, value in ((b"lodScale", b"0.450000"), (b"smallCullThreshold", b"2.000000")):
        pattern = rb'(<'+tag+rb'\s+value=")[^"]*(")'
        raw, count = re.subn(pattern, lambda m: m[1] + value + m[2], raw)
        if count != 1:
            raise ValueError(f"réglage {tag.decode()} absent ou ambigu")
    ET.fromstring(raw)
    return relative, raw


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
    p.add_argument("--shader-overlay", type=absolute, help="pack VSP1 de traductions supplémentaires du même build")
    p.add_argument("--shader-ab", action="store_true", help="afficher le HUD et son panneau de contrôle des pipelines et des paires de shaders")
    p.add_argument("--capture-shader-misses", action="store_true", help="conserver les conteneurs traduits à la volée dans la session")
    p.add_argument("--shader-refusal-cache", type=absolute, metavar="DOSSIER", help="conserver les refus de traduction reconnus, isolés par empreinte de Victory")
    p.add_argument("--prewarm-pipeline", type=pipeline_key, action="append", default=[], help="clé de pipeline à préparer à sa création (8 maximum)")
    p.add_argument("--required-pipeline", type=pipeline_key, action="append", default=[], help="clé dont le premier dessin attend le résultat (8 maximum)")
    p.add_argument("--intro-profile", action="store_true", help="profil expérimental Series S des pools et des lectures ; applique LOD 0,45 et seuil 2 à la copie de --data")
    p.add_argument("--pipeline-manifest", type=absolute, help="liste privée de clés à préparer")
    p.add_argument("--perf-hud", action="store_true", help="afficher les FPS mesurés et la mémoire de Victory au-dessus du jeu")
    p.add_argument("--preview", action="store_true", help="afficher la commande sans créer de dossier ni lancer Wine")
    args = p.parse_args()
    for path in (args.victory, args.title, args.shader_pack, args.shader_overlay, args.pipeline_manifest):
        if path and not path.is_file():
            p.error(f"fichier manquant : {path}")
    for path in (args.bottle, args.engine, args.data):
        if path and not path.is_dir():
            p.error(f"dossier manquant : {path}")
    if args.data and (args.output == args.data or args.data in args.output.parents):
        p.error("le dossier de sortie doit être hors des données à copier")
    if (args.shader_overlay or args.capture_shader_misses or args.shader_refusal_cache) and not args.shader_pack:
        p.error("les options de traduction demandées nécessitent --shader-pack")
    if len(args.prewarm_pipeline) > 8 or len(args.required_pipeline) > 8:
        p.error("huit clés au maximum pour chaque sélection de pipelines")
    settings = None
    if args.intro_profile:
        if not args.data:
            p.error("--intro-profile nécessite les données de la partie à copier avec --data")
        try:
            settings = intro_settings(args.title, args.data)
        except (OSError, ValueError, ET.ParseError) as exc:
            p.error(str(exc))
    args.perf_hud = args.perf_hud or args.shader_ab
    if args.preview:
        run = args.output / "session-preview"
    else:
        args.output.mkdir(parents=True, exist_ok=True)
        run = Path(mkdtemp(prefix="session-", dir=args.output))
        if args.data:
            shutil.copytree(args.data, run / "data")
        if settings:
            (run / "data" / settings[0]).write_bytes(settings[1])
        for folder in ("data", "cache", "captures"):
            (run / folder).mkdir(exist_ok=True)
    config = dict(PROFILE, VICTORY_GAME_DIR=drive_z(args.title.parent), VICTORY_DATA_DIR=drive_z(run/"data"),
                  VICTORY_CACHE_DIR=drive_z(run/"cache"), VICTORY_CAPTURE=drive_z(run/"captures"))
    if args.shader_pack:
        config["VICTORY_READONLY_SHADER_PACK"] = drive_z(args.shader_pack)
    if args.intro_profile:
        config.update(INTRO)
    if args.shader_overlay:
        config["VICTORY_READONLY_SHADER_OVERLAY"] = drive_z(args.shader_overlay)
    if args.shader_ab:
        if not args.preview:
            (run / "shader-control.txt").write_text("0 all\n", encoding="ascii")
        config["VICTORY_SHADER_CONTROL_FILE"] = drive_z(run / "shader-control.txt")
    if args.capture_shader_misses:
        if not args.preview:
            (run / "shader-misses").mkdir()
        config["VICTORY_SHADER_MISSES_DIR"] = drive_z(run / "shader-misses")
    if args.shader_refusal_cache:
        if not args.preview:
            args.shader_refusal_cache.mkdir(parents=True, exist_ok=True)
        config["VICTORY_SHADER_REFUSAL_FILE"] = drive_z(args.shader_refusal_cache / (sha256(args.victory) + ".lane-mask.v1"))
    if args.prewarm_pipeline:
        config["VICTORY_PREWARM_PIPELINE_KEYS"] = ",".join(args.prewarm_pipeline)
    if args.required_pipeline:
        config["VICTORY_REQUIRED_PIPELINE_KEYS"] = ",".join(args.required_pipeline)
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
    record = {"mode": "run", "command": command, "perf_hud": args.perf_hud, "shader_ab": args.shader_ab,
              "intro_profile": args.intro_profile, "capture_shader_misses": args.capture_shader_misses}
    for name, pack in (("readonly_shader_pack", args.shader_pack), ("readonly_shader_overlay", args.shader_overlay)):
        if pack:
            record[name] = str(pack)
            record[name + "_sha256"] = sha256(pack)
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
                    hud = PerformanceHUD(hud_binary, run, args.victory, monitor.pid, shader_ab=args.shader_ab)
                except OSError as error:
                    print(f"Encart indisponible : {error}", file=sys.stderr)
            rc = monitor.wait()
        finally:
            if hud is not None:
                hud.close()
            for sig, handler in handlers.items():
                signal.signal(sig, handler)
    record["exit_code"] = rc
    (run / "launch.json").write_text(json.dumps(record, indent=2))
    return rc


if __name__ == "__main__":
    raise SystemExit(main())
