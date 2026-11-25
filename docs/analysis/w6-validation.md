# W6 — validation des signatures DXIL sur la L4

## Conditions de mesure

Le 25 novembre 2026, les exécutables Windows ont été construits depuis des
arbres Git propres avec `cargo build --locked --profile diag -p xtp`. La
L4 a exécuté le relevé suivant pour `2545065` et `7b63628`,
avec le validateur 1.9 du SDK 26100 (dxil.dll 1.9.2602.17).

```powershell
.\xtp.exe shader-survey D:\game\shaders.rpf --validate --host-lanes 32-32 --json releve.json
```

Le pack de 411 695 600 octets a pour BLAKE3
`2b023331518b1b7827aa7c9e478e78888c0d52c705fd980ddc04ae9ee14d20b5`.
Les JSON permettent de joindre chaque programme par le couple
(`record`, `program`). L’empreinte `output` couvre les octets du
conteneur DXIL avant signature par le validateur ainsi que les tables de
liaison. Les relevés et les journaux sont sur la L4 dans
`D:\xtp-data\w6-841901`, préfixés par la révision.

## Gain de 7b63628

**117 validations gagnées, aucune perdue.** Les 26 698 clés sont uniques
et communes aux deux relevés. Les deux révisions traduisent 25 441
programmes. Les 1 257 autres échouent avant le validateur.

| Premier verdict de dxil.dll | `2545065` | `7b63628` | Programmes concernés avant le correctif |
|---|---:|---:|---|
| Accepté | 25 253 | 25 370 | Les 25 253 restent acceptés avec la même empreinte |
| Signature d’entrée ISG1 | 27 | 0 | 27 PS |
| Signature de sortie OSG1 | 34 | 0 | 26 VS, 8 MS |
| Interpolations incompatibles dans une ligne | 56 | 0 | 36 PS, 15 VS, 5 MS |
| Sorties de mesh dépassant 32 768 octets | 71 | 71 | Les mêmes 71 MS |

Les 117 empreintes qui changent appartiennent exactement aux 117 programmes
qui passent de refusé à accepté. Pour chacun des 24 275 succès du témoin
historique, l’empreinte à `2545065` est identique à celle à `7b63628`.

La création de pipelines D3D12 a un comptage distinct. À la remise de ce
lot, la commande `translate --validate --pipelines --host-lanes 32-32`
avait été arrêtée à `2545065` avant son bilan. L’exécution à `7b63628`
restait à faire. La tâche Hangar `6d512c1f` reprend les deux mesures avec
les exécutables conservés sur la L4.

Cette vérification lit les deux JSON depuis le répertoire des relevés.

```python
import json
from pathlib import Path

def programmes(revision):
    releve = json.loads(Path(revision + ".json").read_bytes())
    lignes = releve["list"]
    index = {(p["record"], p["program"]): p for p in lignes}
    assert len(index) == len(lignes) == 26_698
    return index

avant, apres = map(programmes, ["2545065", "7b63628"])
assert avant.keys() == apres.keys()
valides = {k for k, p in avant.items() if p.get("validator") == "ok"}
gagnes = {k for k, p in apres.items() if p.get("validator") == "ok"} - valides
modifies = {k for k in avant if avant[k].get("output") != apres[k].get("output")}
assert len(valides) == 25_253 and all(apres[k]["validator"] == "ok" for k in valides)
assert modifies == gagnes and len(gagnes) == 117
```

## Origine des 24 275 validations

Le fichier historique `D:\xtp-data\w6-before.json` correspond à
`37ee7ea`. Son empreinte globale des sorties
(`9255acc313a2b4dd337d2bceef409118d5b0753c09e3568bbbd9e2358a305b06`),
le pack, les 24 433 traductions et les 158 refus concordent avec le relevé
consigné dans la tâche Hangar `7452518e`.

Les 24 275 programmes de ce témoin restent acceptés à `2545065`. Entre
`37ee7ea` et `2545065`, l’empreinte de 824 d’entre eux avait déjà changé
(558 CS, 258 PS, 8 MS) et 978 programmes supplémentaires avaient été
acceptés. La base de comparaison du correctif est donc de **25 253**
validations.

## Accès au tas dans deux programmes du pack

Les deux programmes ont été traduits à `7b63628`, pour 32–32 lanes.
Les opérations ci-dessous sont les appels du module désassemblé avec
`dxc -dumpbin`. Les deux validateurs ont reçu les mêmes octets émis.

| Programme (`record/program`) | Opérations et ressource | ShaderFlags DXIL | SFI0 | dxil.dll 1.6 (PC) | dxil.dll 1.9 (L4) |
|---|---|---:|---:|---|---|
| VS de skinning `0x54c00/0` | `createHandleFromHeap` puis `annotateHandle` en `RWStructuredBuffer` de pas 12. Trois `rawBufferStore.i32` sur ce handle, aux offsets 0, 4 et 8. | `0x40100010` | `0x02008000` | Refus PSV0 | Accepté |
| MS d’ombres de maillage skinné `0x49fec/0` | Neuf `createHandleFromHeap`. Lectures SRV par `rawBufferLoad` et `bufferLoad`. Les écritures `storeVertexOutput` et `emitIndices` portent sur la géométrie. | `0x40000010` | `0x02000000` | Accepté | Accepté |

Sur les 9 181 MS traduits à `7b63628`, les 4 653 empreintes distinctes
ont été reproduites sur la L4. Leurs modules comptent zéro appel de
stockage de buffer ou de texture, d’atomique UAV ou de mise à jour de
compteur UAV. Le VS fournit le cas positif d’écriture par le tas.

Le VS écrit dans un UAV du tas avec le bit 16 des ShaderFlags et le bit
`0x4` de SFI0 tous deux à zéro. Avec le SDK 22621, le validateur 1.6
signale uniquement `Pipeline State Validation`. Son assembleur reconstruit
une partie PSV0 de 288 octets (272 dans la traduction). Remplacer cette
seule partie fait accepter le VS en 1.6, à DXIL et SFI0 identiques.
Le témoin réassemblé avec le PSV0 original conserve le refus. La tâche
précédente avait aussi observé le diagnostic PSV0 en 1.6 pour trois tests
préexistants et pour les 35 sorties dès la rotation 0.

## Contrôle des signatures synthétiques

Sur la L4, le relevé du correctif de tri `2d41fc1` est identique octet
pour octet à celui de son premier parent `a012d2c`. Les deux JSON ont
pour SHA-256 `32cbc24159467d1091ce0b8f820606cbada02565ed26af02397e7e82eba93c4b`.
Les 25 768 traductions, dont 25 697 acceptées, conservent leurs empreintes
et leurs verdicts.

Les 11 tests du filtre `validator` passent sur la ferme Windows.
`IDxcVersionInfo::GetVersion` renvoie 1.9 pour la DLL x64 du SDK 26100.
Dans les signatures à 35 éléments, les rotations 15 et 16 donnent
respectivement les indices [0, 1] et [1, 0] pour les deux distances de
clipping dans ISG1 et OSG1. Le validateur accepte les deux ordres dans
leurs conteneurs respectifs.

Pour PSG1, le mesh témoin avec un seul `SV_PrimitiveID` passe. En ajouter
un deuxième dans le même registre permet de tester la permutation des lignes
de même clé dans le conteneur émis. Le test vérifie leurs deux ordres et
le refus attendu de la sémantique répétée.
