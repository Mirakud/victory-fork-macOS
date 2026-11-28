"""Lance Victory avec D3DMetal 4 et le moniteur mémoire sur un dossier de jeu local préparé."""
import argparse
from contextlib import ExitStack, contextmanager
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import select
import signal
import socket
import shutil
import subprocess
import sys
import threading
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


# Le profil prison conserve les passes du moteur 6c32891.
PRISON = {
    "VICTORY_TEXTURE_NO_WAIT": "1", "VICTORY_FAST_MIRROR_COMPARE": "1",
    "VICTORY_SMALL_SPARSE_READ": "1", "VICTORY_SPARSE_READ_RUNS": "1",
    "VICTORY_STORAGE_VIEW_CACHE": "1", "VICTORY_RELAY_STALE_WRITES": "1",
    "VICTORY_DECLINE_RECORDED_NOOPS": "1", "VICTORY_TRACE_STALLED_MESH": "1",
    "VICTORY_CARGEN_RESERVE": "1", "VICTORY_MAP_POOL_RESERVE": "building",
    "VICTORY_LOG": (
        "warn,"
        "victory_gpu::memory_watch=info,"
        "victory_gpu::host=info,"
        "victory_gpu::shadow=info,"
        "victory_hle::modules::xmem=info"
    ),
    "VICTORY_PREWARM_PIPELINE_KEYS": (
        "f097e3daff75e2fafc8403a56be48e19,"
        "5d619a339c8cbd8aef13855864f4fc33,"
        "c29be1816e455b60991b959396a442af,"
        "50ece4da57f39c0ef6090e9de8a0f577,"
        "9b1e557f10ca3e249881765fe527e51b,"
        "62a5c8deb38de8e3fcc8a87059a8a5ea,"
        "cec72f2c49336e8f454a689750848858,"
        "7e8ac42c301540aed279377e2bf25823"
    ),
    "VICTORY_REQUIRED_PIPELINE_KEYS": (
        "c41b40db9a2533b68af018cd90ccdb26,"
        "b21dd30ebf9356697ff67def8475381a,"
        "ad4724939f988a4e209fcf3e24eef61b,"
        "bbaf821d76e9fa143059b70a59c4e54a,"
        "750bf0bbce5eac8cce2dbca9ae4baef4,"
        "0b7a96fcae76f7001affdb9e0eb6e887,"
        "2320db8841d21997b266cc4e4a789032"
    ),
}


# Parcours Series S validé sur la copie de sauvegarde de la session 013918.
SERIES_S = {
    "VICTORY_PIPELINE_WORKERS": "4",
    "VICTORY_TEXTURE_BUDGET_MB": "512",
    "VICTORY_WORLD_TEXTURE_BUDGET_MB": "1536",
    "VICTORY_BVH_PAGE_CACHE_MB": "768",
    "VICTORY_PRISON_MATERIAL_TEXTURES": "1",
    "VICTORY_TRACE_AS_HEADER": "1",
    "VICTORY_LIST_SLOTS": "c94c4c7b5775cc2b,55e52a40b5d79b20,a885ba5d98ce46bf,1407289e381df336",
    "VICTORY_LOG": PRISON["VICTORY_LOG"] + ",victory_gpu::texture=info,victory_gpu::capture=info,victory_gpu::pipeline=info",
    "VICTORY_KEYS": "40:Enter,67:Enter",
    "VICTORY_CAPTURE_TARGET_COMMANDS": "1",
    "VICTORY_LIST_SLOTS": "c94c4c7b5775cc2b,55e52a40b5d79b20,a885ba5d98ce46bf,1407289e381df336",
    "VICTORY_LOG": PRISON["VICTORY_LOG"] + ",victory_gpu::texture=info,victory_gpu::capture=info,victory_gpu::pipeline=info",
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


@contextmanager
def dualsense_presence(binary, run):
    state = run / "sony-present"
    with (run / "sony-detection.log").open("wb") as log:
        try:
            helper = subprocess.Popen([str(binary), "--watch-dualsense", str(state)],
                                      stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log)
        except OSError as error:
            print(f"Détection Sony indisponible ({error}), lecture WinMM conservée.", file=sys.stderr)
            yield False, 0
            return
        stopping = threading.Event()
        watcher = None
        try:
            readable, _, _ = select.select([helper.stdout], [], [], 5)
            line = helper.stdout.readline() if readable else b""
            handshake = re.fullmatch(rb"ready(?: ([0-9]{1,5}))?\n", line)
            port = int(handshake[1] or 0) if handshake else 0
            ready = handshake is not None and port <= 65535
            if ready:
                def recover_on_exit():
                    helper.wait()
                    if not stopping.is_set():
                        # Allow WinMM to discover the pad if native detection stops mid-run.
                        temporary = state.with_suffix(".restore")
                        try:
                            temporary.write_text("1\n", encoding="ascii")
                            temporary.replace(state)
                            log.write(b"Detection HID interrompue, lecture WinMM reactivee.\n")
                        except OSError as error:
                            log.write(f"Impossible de restaurer la lecture WinMM : {error}\n".encode())
                        log.flush()

                watcher = threading.Thread(target=recover_on_exit, name="victoire-detection-sony", daemon=True)
                watcher.start()
            else:
                print("Détection Sony indisponible, lecture WinMM conservée.", file=sys.stderr)
                helper.terminate()
            yield ready, port if ready else 0
        finally:
            stopping.set()
            helper.stdin.close()
            helper.stdout.close()
            try:
                helper.wait(timeout=2)
            except subprocess.TimeoutExpired:
                helper.kill()
                helper.wait()
            if watcher:
                watcher.join()


def main():
    with ExitStack() as resources:
        return run_profile(resources)


def run_profile(resources):
    p = argparse.ArgumentParser(description=__doc__)
    for name in ("victory", "engine", "bottle", "output"):
        p.add_argument("--" + name, type=absolute, required=True)
    # Keep a staged title beside its config and archives even if the EXE is a symlink.
    p.add_argument("--title", type=lambda path: Path(path).expanduser().absolute(), required=True)
    p.add_argument("--data", type=absolute, help="dossier de données Victory à copier pour cet essai")
    p.add_argument("--shader-pack", type=absolute, help="pack privé compatible avec le binaire utilisé")
    p.add_argument("--shader-overlay", type=absolute, help="pack VSP1 de traductions supplémentaires du même build")
    p.add_argument("--shader-store", type=absolute, metavar="DOSSIER", help="répertoire privé de traductions préparées pour le même build")
    p.add_argument("--shader-names", type=absolute, metavar="JSON", help="catalogue privé de noms de shaders pour le panneau A/B")
    p.add_argument("--shader-ab", action="store_true", help="afficher le HUD et son panneau de contrôle des pipelines et des paires de shaders")
    p.add_argument("--capture-shader-misses", action="store_true", help="conserver les conteneurs traduits à la volée dans la session")
    p.add_argument("--shader-refusal-cache", type=absolute, metavar="DOSSIER", help="conserver les refus de traduction reconnus, isolés par empreinte de Victory")
    p.add_argument("--prewarm-pipeline", type=pipeline_key, action="append", default=[], help="clé de pipeline à préparer à sa création (8 maximum)")
    p.add_argument("--required-pipeline", type=pipeline_key, action="append", default=[], help="clé dont le premier dessin attend le résultat (8 maximum)")
    profiles = p.add_mutually_exclusive_group()
    profiles.add_argument("--prison-profile", "--prison", action="store_true", help="réglages de la session jouable 030001, panneau shaders et mémoire gérée par macOS")
    profiles.add_argument("--prison-peau", action="store_true", help="profil de la cour avec visage et cou, entrées et un travailleur de chargement (session 124802)")
    profiles.add_argument("--intro-profile", action="store_true", help="profil expérimental Series S des pools et des lectures ; applique LOD 0,45 et seuil 2 à la copie de --data")
    profiles.add_argument("--series-s", action="store_true", help="profil Series S, DualSense native, quatre travailleurs et 1 536 Mio de textures dans la scène")
    p.add_argument("--bvh-scan-records", type=absolute, metavar="DOSSIER", help="scans BVH privés préparés pour ce titre")
    p.add_argument("--belobug-cascade-pack", type=absolute, metavar="FICHIER", help="pack privé compatible des shaders d’ombres en cascades")
    p.add_argument("--pipeline-manifest", type=absolute, help="liste privée de clés à préparer")
    p.add_argument("--perf-hud", action="store_true", help="afficher les FPS mesurés et la mémoire de Victory au-dessus du jeu")
    p.add_argument("--preview", action="store_true", help="afficher la commande sans créer de dossier ni lancer Wine")
    args = p.parse_args()
    for path in (args.victory, args.title, args.shader_pack, args.shader_overlay, args.pipeline_manifest, args.shader_names, args.belobug_cascade_pack):
        if path and not path.is_file():
            p.error(f"fichier manquant : {path}")
    for path in (args.bottle, args.engine, args.data, args.shader_store, args.bvh_scan_records):
        if path and not path.is_dir():
            p.error(f"dossier manquant : {path}")
    if args.data and (args.output == args.data or args.data in args.output.parents):
        p.error("le dossier de sortie doit être hors des données à copier")
    if (args.shader_overlay or args.capture_shader_misses or args.shader_refusal_cache) and not args.shader_pack:
        p.error("les options de traduction demandées nécessitent --shader-pack")
    if len(args.prewarm_pipeline) > 8 or len(args.required_pipeline) > 8:
        p.error("huit clés au maximum pour chaque sélection de pipelines")
    settings = None
    prison = args.prison_profile or args.prison_peau or args.series_s
    if prison:
        args.shader_ab = True
        args.capture_shader_misses = bool(args.shader_pack)
        if args.shader_pack and not args.shader_refusal_cache:
            args.shader_refusal_cache = args.output / "shader-refusals"
    if args.intro_profile or prison:
        if not args.data:
            p.error("le profil choisi nécessite les données de la partie à copier avec --data")
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
    input_socket = hud_binary = None
    if args.perf_hud and not args.preview:
        from victory_perf_feed import PerformanceHUD, build_hud
        hud_binary = build_hud(TOOLS / "victory_perf_hud.swift")
    if args.shader_pack:
        config["VICTORY_READONLY_SHADER_PACK"] = drive_z(args.shader_pack)
    if args.intro_profile:
        config.update(INTRO)
    elif prison:
        config.update(PRISON)
        config["VICTORY_CONFIRM_FILE"] = drive_z(run / "confirm")
        config["VICTORY_CAPTURE_FILE"] = drive_z(run / "capture")
    if args.series_s:
        config.update(SERIES_S)
        config["VICTORY_ROOM_LIGHT_FILE"] = drive_z(run / "lampes-cellule")
        config["VICTORY_CAPTURE_TARGETS_FILE"] = drive_z(run / "capture-targets")
    if args.prison_peau:
        config.pop("VICTORY_TRACE_STALLED_MESH")
        config.update(VICTORY_PARALLEL_CARD_DIGEST="1", VICTORY_CARD_REHASH_RUNS="1",
                      VICTORY_PED_MATERIAL_TEXTURES="1", VICTORY_MEMORY_MAP="0", VICTORY_STORAGE_WORKERS="1",
                      VICTORY_KEYS="35:Enter", VICTORY_CAPTURE_TARGETS_FILE=drive_z(run / "capture-targets"))
    if args.prison_peau or args.series_s:
        if args.preview:
            hud_port, ready, sony_port = 0, True, 0
        else:
            input_socket = resources.enter_context(socket.socket(socket.AF_INET, socket.SOCK_DGRAM))
            input_socket.bind(("127.0.0.1", 0))
            input_socket.setblocking(False)
            hud_port = input_socket.getsockname()[1]
            ready, sony_port = resources.enter_context(dualsense_presence(hud_binary, run))
        config["VICTORY_INPUT_HUD_PORT"] = str(hud_port)
        if ready:
            config["VICTORY_DUALSENSE_PRESENT_FILE"] = drive_z(run / "sony-present")
            if args.series_s and (sony_port or args.preview):
                config["VICTORY_DUALSENSE_NATIVE_PORT"] = str(sony_port)
    if args.bvh_scan_records:
        config.update(VICTORY_BVH_SCAN_RECORDS=drive_z(args.bvh_scan_records),
                      VICTORY_BVH_BUILDER="1", VICTORY_BVH_QUARANTINE="1")
    if args.belobug_cascade_pack:
        config["VICTORY_BELOBUG_CASCADE_PACK"] = drive_z(args.belobug_cascade_pack)
    if args.shader_store:
        config["VICTORY_READONLY_SHADER_STORE"] = drive_z(args.shader_store)
    if args.shader_names and args.shader_ab and not args.preview:
        shutil.copyfile(args.shader_names, run / "shader-names.json")
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
    memory = ["--no-memory-limit"] if prison else ["--max-wired-mib", "9216"]
    command = [sys.executable, str(TOOLS/"monitor_xcl_run.py"), *memory,
               "--report-peak-used-percent", "95", "--poll-interval", "1.0" if prison else "0.05",
               "--telemetry-path", str(run/"telemetry.jsonl"), "--", *child]
    if args.preview:
        print(shlex.join(command))
        if args.perf_hud:
            print("Encart natif demandé (FPS, mémoire de Victory, mirakud).")
        return 0
    env = dict(os.environ, UNCORK_SIKARUGIR_ENGINE=str(args.engine), VICTORY_MAC_BOTTLE=str(args.bottle))
    record = {"mode": "run", "command": command, "perf_hud": args.perf_hud, "shader_ab": args.shader_ab,
              "intro_profile": args.intro_profile, "prison_profile": args.prison_profile, "prison_peau": args.prison_peau, "series_s": args.series_s, "capture_shader_misses": args.capture_shader_misses}
    if args.series_s or args.prison_peau:
        record["victory_sha256"] = sha256(args.victory)
    if args.bvh_scan_records:
        record["bvh_scan_records"] = str(args.bvh_scan_records)
    for name, pack in (("readonly_shader_pack", args.shader_pack), ("readonly_shader_overlay", args.shader_overlay),
                       ("belobug_cascade_pack", args.belobug_cascade_pack), ("pipeline_manifest", args.pipeline_manifest)):
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
                    hud = PerformanceHUD(hud_binary, run, args.victory, monitor.pid, shader_ab=args.shader_ab, input_socket=input_socket)
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
