# Victory sur macOS

Victory est une couche de compatibilité qui traduit les appels d’un titre Xbox Series vers Direct3D 12. Cette branche expérimentale vise le rendu sur les Mac Apple Silicon, avec Wine Sikarugir et D3DMetal 4. Sur un Mac M3 de 16 Gio, le profil Series S permet de dépasser le carton d’introduction et de voir une rue nocturne, ses feux et un personnage. Le parcours 200227 atteint ensuite une voiture avec sa portière ouverte, puis un passage couvert. La ville apparaît sans bandes cinématiques avec une jauge verte en haut à gauche, puis les bandes reviennent sur un plan de côte éclairée. La scène reste trop lente pour être jouable et les parcours se terminent par un arrêt du titre.

![Plan suivant, voiture avec la portière ouverte, essai 200227](docs/macos/voiture.png)

![Rue de nuit, personnage au premier plan et profondeur active, essai 200227](docs/macos/rue-profondeur.png)

| Rue au premier plan | Feux et matériaux pendant la transition | Personnage et horizon |
|---|---|---|
| ![Rue, première vue](docs/macos/rue-1.png) | ![Rue, deuxième vue](docs/macos/rue-2.png) | ![Rue, troisième vue](docs/macos/rue-3.png) |

Les trois vues de référence proviennent du parcours 191652. Les surfaces très sombres et les matériaux transitoires restent visibles. Les deux grandes images montrent la rue puis la voiture dans le parcours 200227, avec restauration de profondeur active.

## Les neuf plans de 200227

La jauge des trames 1856 et 1864 apparaît encore pendant la cinématique, ce que le joueur a confirmé. Les bandes reviennent au plan de la côte. La prise de contrôle du personnage reste à vérifier. Chaque vignette ouvre la capture complète de 960 × 540.

| 1808 · Transition sombre | 1816 · Personnage dans la rue | 1824 · Rue et feux |
|---|---|---|
| [![Transition sombre](docs/macos/frame-1808.png)](docs/macos/frame-1808.png) | [![Personnage dans la rue](docs/macos/frame-1816.png)](docs/macos/frame-1816.png) | [![Rue et feux](docs/macos/frame-1824.png)](docs/macos/frame-1824.png) |

| 1832 · Voiture, portière ouverte | 1840 · Horizon orange | 1848 · Passage couvert à piliers |
|---|---|---|
| [![Voiture, portière ouverte](docs/macos/frame-1832.png)](docs/macos/frame-1832.png) | [![Horizon orange](docs/macos/frame-1840.png)](docs/macos/frame-1840.png) | [![Passage couvert à piliers](docs/macos/frame-1848.png)](docs/macos/frame-1848.png) |

| 1856 · Ville et jauge verte | 1864 · Ciel éclairé avec la jauge | 1872 · Côte, retour des bandes cinématiques |
|---|---|---|
| [![Ville et jauge verte](docs/macos/frame-1856.png)](docs/macos/frame-1856.png) | [![Ciel éclairé avec la jauge](docs/macos/frame-1864.png)](docs/macos/frame-1864.png) | [![Côte, retour des bandes cinématiques](docs/macos/frame-1872.png)](docs/macos/frame-1872.png) |

## État mesuré le 26 novembre 2026

Les essais ont utilisé un M3 de 16 Gio, Sikarugir Wine 10.0 révision 6, D3DMetal 4.0 bêta 3 et le profil Series S. Les médianes portent sur les relevés du journal dont la moyenne atteint 1 400 dessins par image. Le nombre de relevés, la caméra et l’avancement de la mission varient entre parcours. La différence de médianes ne mesure pas à elle seule un gain de performance.

| Essai | Relevés retenus | Médiane par image | Dont copies CPU | Pic de mémoire câblée macOS | Dernière trame du titre |
|---|---:|---:|---:|---:|---:|
| 191652, rue visible | 19 | 14,36 s | 9,08 s | 8 614 Mio | 963 |
| 195805, restauration de profondeur active | 6 | 6,85 s | 4,25 s | 8 662 Mio | 748 |
| 200227, jusqu’au plan du littoral | 36 | 10,02 s | 6,64 s | 9 126,7 Mio | 836 |

Les parcours 191652 et 195805 s’arrêtent sur la surveillance du titre. En 200227, le titre déclenche un point d’arrêt volontaire `0x80000003`. Son rapport à la trame 836 indique `INSTANCE_LIST 45000/45000`, contre `30819/45000` en 191652. Le lien causal avec la saturation de cette liste reste à établir. La garde mémoire était fixée à 9 216 Mio. Le relevé en jeu de 195805 atteint **1,0 restauration de profondeur par image**. Sur le banc D3DMetal séparé, trois tests passent. Les marches R16 0, 385, 32 768 et 65 535 produisent en D24 les valeurs hexadécimales 000000, 018101, 80007F et FFFFFF ; le stencil reste à 5A sur les quatre pixels. Le second dessin préserve également les profondeurs D32 de référence. [Sources du banc](crates/victory-gpu/src/host/depth_plane_readback.rs)

## Construire

La source intègre le candidat Victory `c9498925311320302f59e381f51e78aabe8a55ea`. Les tests de profondeur proviennent de `95732f4` et `b6bf7c8`. La cadence de capture diagnostique provient de `4541df7`. En mode `VICTORY_CAPTURE_WORLD=only`, `VICTORY_CAPTURE_WORLD_INTERVAL=1` enregistre un PNG à chaque présentation du monde. Les intervalles de 1 à 64 sont acceptés. La recette du candidat mesuré `c949892` reste la référence de 195805 et de 200227. Le README amont conserve le détail des composants, le modèle d’exécution et les instructions du projet. [Documentation amont](README.amont.md) · [Architecture](docs/architecture.md) · [État du ray tracing](docs/fork-ray-tracing.md)

Dans un terminal de développement Windows x64 avec le SDK Windows, Rust MSVC, Python 3 et le DirectX Shader Compiler de Microsoft sur le PATH, lancer les commandes suivantes. Le script compile les deux noyaux HLSL du projet en DXIL local avant que Rust n’embarque leurs octets. Le paramètre `--dxc` accepte un chemin explicite.

```powershell
python tools/shaders/prepare.py
cargo build --locked --release -p victory --features diagnostics
cargo test --locked -p victory-gpu --features diagnostics depth_plane_readback -- --test-threads=1 --nocapture
```

Les deux tests de dessin ont besoin d’un périphérique D3D12 et des composants de compilation du runtime. Le profil diagnostics permet la capture et la télémétrie employées pendant les essais. Le dépôt distribue les sources des shaders ; DXC et les produits générés restent locaux.

## Lancer un essai sur Mac

Le lanceur accepte des chemins locaux, y compris un volume externe monté. Il attend un moteur Sikarugir 10.0 révision 6 avec D3DMetal 4.0 bêta 3 installé, une bottle déjà configurée avec son disque Z sur la racine du Mac, le binaire Windows construit ci-dessus et le répertoire du titre préparé par son propriétaire. Le SHA des composants du runtime est contrôlé à l’ouverture. Python 3.11 ou ultérieur est nécessaire sur le Mac. Les données de test, le titre, la bottle, les runtimes et les paquets de shaders dérivés sont à fournir localement.

```sh
python3 tools/macos/run.py \
    --victory "$VICTORY_EXE" --title "$TITLE_EXE" \
    --engine "$SIKARUGIR_ENGINE" --bottle "$VICTORY_BOTTLE" \
    --output "$RUNS_DIR" --preview
```

Retirer `--preview` pour exécuter. `--data` copie les données privées d’un profil dans la nouvelle session. Les options `--shader-pack` et `--pipeline-manifest` acceptent des caches et priorités générés localement pour une version compatible. Au choix de profil du jeu, confirmer Player avec Entrée. Chaque session enregistre le journal et la télémétrie. Le moniteur arrête le groupe de processus à 9 216 Mio câblés ou dès l’erreur Metal de mémoire insuffisante. Le lanceur reprend les réglages de base de la rue, tandis que les profils, priorités spécifiques et entrées utilisées lors du développement restent propres aux essais.

## Licence et contenu

Le code Victory est distribué sous GPL-3.0-or-later. Les attributions et notices d’origine sont conservées. Les tables constantes du format BCPack gardent leur statut particulier documenté en amont. Les copies d’affichage portent la signature « mirakud » en bas à droite. Le générateur contrôle la conservation de chaque pixel hors de cette signature. Les captures illustrent exclusivement les essais de ce fork privé ; la licence du code ne s’applique pas aux images du titre. [Licence](LICENSE) · [Règles amont relatives aux ressources](docs/asset-policy.md)
