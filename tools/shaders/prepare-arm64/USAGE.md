# Préparer ses shaders sur un Mac Apple Silicon

Ce programme traduit sur le CPU du Mac les conteneurs de shaders de votre jeu en DXIL et enregistre leurs informations de liaison au format VSH1. Un pack VSP1 regroupe ces traductions pour Victory. La compilation des pipelines par D3DMetal reste assurée par le pilote.

Depuis la racine du dépôt, compilez le préparateur et le lecteur Victory depuis la même révision. Les commandes suivantes correspondent au lecteur de diagnostic en mode release, avec le profil D3DMetal 4 mesuré (wave 32, descripteur nul 999 999). Le lecteur et le préparateur doivent recevoir les mêmes variables de traduction `VICTORY_*`, notamment une éventuelle valeur de `VICTORY_SHADER_OPT`. Avec un lecteur release dépourvu de la fonctionnalité Cargo `diagnostics`, ajoutez `--no-default-features --features diagnostics` à la commande de compilation du préparateur.

```sh
work="$HOME/Library/Caches/victory-shader-tools"
mkdir -p "$work"
cargo build --locked --release --manifest-path tools/shaders/prepare-arm64/Cargo.toml \
  --target-dir "$work/build"
prep="$work/build/release/victory-prepare-shaders"
```

La commande `"$prep" identity` affiche la clé que le préparateur inscrit dans les octets 8 à 39 des packs VSP1. Le profil public avec diagnostics et réglages de traduction par défaut utilise `d6b56b368b0f6ce27b5b819dc4b894255b9ef61d1933c67010652f6a2cb6bb62`. Les packs de l’essai privé D3 portent la clé `578b25415d58675d5ba8637836bf4f93baf92f43cb501d6a0365e01c3cec4218`. Pour utiliser ces mêmes shaders avec le lecteur public, reconstruisez votre pack et votre overlay depuis vos conteneurs personnels avec les commandes suivantes.

Pour amorcer un premier pack, fournissez un fichier SGD5 personnel ou une archive du jeu contenant un SGD5. `extract` lit l’archive en mémoire et s’arrête après le nombre demandé de conteneurs distincts. Pour une session qui possède déjà un dossier `shader-misses`, utilisez directement ce dossier comme valeur de `inputs` et passez à `pack`.

```sh
inputs="$work/containers"
"$prep" extract "/chemin/vers/votre/archive" "$inputs" 32
"$prep" pack "$inputs" "$work/translations" "$work/premier.vsp"
"$prep" verify "$inputs" "$work/premier.vsp"
```

`pack` inclut les traductions réussies, signale chaque refus et relit toutes les entrées avec le lecteur de Victory avant de créer le fichier demandé. `verify` vérifie le BLAKE3, la clé de construction et chaque conteneur associé. Un résultat `verified=N fallback=0` confirme que les N traductions ont été décodées depuis le pack. Gardez vos fichiers hors du dépôt. Un changement au code source du traducteur peut changer la clé de construction même si le DXIL obtenu reste identique.

Pour enrichir ce pack, lancez votre session avec les options macOS `--shader-pack "$work/premier.vsp" --capture-shader-misses`. À sa fermeture, traduisez les captures puis assemblez votre overlay. Le script Python nécessite le module `blake3` ou la commande `b3sum`.

```sh
inputs="/chemin/vers/la/session/shader-misses"
"$prep" warm "$inputs" "$work/translations"
python3 tools/shaders/pack_overlay.py --shader-pack "$work/premier.vsp" \
  --inputs "$inputs" --translations "$work/translations" --output "$work/overlays"
```

Passez le chemin `current.vsp` imprimé par le script au prochain lancement avec `--shader-overlay`, en conservant le même pack principal.
