# W6 — DXIL signature validation on the L4

## Measurement conditions

On 25 November 2026, the Windows executables were built from clean Git
trees with `cargo build --locked --profile diag -p xtp`. The L4 ran the
following survey for `2545065` and `7b63628`,
with the 1.9 validator from SDK 26100 (dxil.dll 1.9.2602.17).

```powershell
.\xtp.exe shader-survey D:\game\shaders.rpf --validate --host-lanes 32-32 --json releve.json
```

The 411,695,600-byte pack has the BLAKE3 hash
`2b023331518b1b7827aa7c9e478e78888c0d52c705fd980ddc04ae9ee14d20b5`.
The JSON files let each program be joined by the (`record`, `program`)
pair. The `output` fingerprint covers the bytes of the DXIL container
before the validator signs it, as well as the binding tables. The surveys
and logs are on the L4 in `D:\xtp-data\w6-841901`, prefixed by the
revision.

## Gain of 7b63628

**117 validations gained, none lost.** The 26,698 keys are unique
and shared by both surveys. Both revisions translate 25,441
programs. The other 1,257 fail before the validator.

| First dxil.dll verdict | `2545065` | `7b63628` | Programs affected before the fix |
|---|---:|---:|---|
| Accepted | 25,253 | 25,370 | The 25,253 stay accepted with the same fingerprint |
| ISG1 input signature | 27 | 0 | 27 PS |
| OSG1 output signature | 34 | 0 | 26 VS, 8 MS |
| Incompatible interpolations in one row | 56 | 0 | 36 PS, 15 VS, 5 MS |
| Mesh outputs exceeding 32,768 bytes | 71 | 71 | The same 71 MS |

The 117 fingerprints that change belong exactly to the 117 programs
that go from refused to accepted. For each of the 24,275 successes of the
historical reference, the fingerprint at `2545065` is identical to the one at `7b63628`.

This check reads both JSON files from the survey directory.

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

## D3D12 pipeline creation on the NVIDIA L4

Both revisions created **24,811 pipelines** on the L4 (driver 596.86).
After input deduplication, `7b63628` gained **115 validations**
(102 VS and PS combined, 13 MS). Both runs used the same pack and
validator as the survey, from `D:\xtp-data\w6-841901`.

```powershell
$env:XTP_DXIL = 'C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64\dxil.dll'
.\2545065.exe translate D:\game\shaders.rpf --validate --pipelines --host-lanes 32-32
.\7b63628.exe translate D:\game\shaders.rpf --validate --pipelines --host-lanes 32-32
```

Both commands exited with code 0. The directory above contains each run's
stdout in `<revision>-6d512c1f-translate-verified.log` and its exit code
in the adjacent `.log.exit` file.

`translate` hashes each input container's DXIL part with BLAKE3 and skips
repeat hashes before enumerating native programs. Each run read
26,503 containers, skipped 614 duplicates, and translated all 26,084
remaining native programs to IR. The survey counts 26,698 program
occurrences by record and program index.

| Revision | Stage | Programs reaching emission | DXIL emitted | Validated | Pipelines created |
|---|---|---:|---:|---:|---:|
| `2545065` | VS + PS | 14,247 | 13,844 | 13,742 | 13,844 |
| `2545065` | CS | 2,617 | 2,319 | 2,319 | 2,303 |
| `2545065` | MS | 8,765 | 8,664 | 8,580 | 8,664 |
| `7b63628` | VS + PS | 14,247 | 13,844 | 13,844 | 13,844 |
| `7b63628` | CS | 2,617 | 2,319 | 2,319 | 2,303 |
| `7b63628` | MS | 8,765 | 8,664 | 8,593 | 8,664 |

Each emitted shader gets its own pipeline creation attempt before validation.
VS and MS use a stub pixel shader, PS uses a matching stub vertex shader,
and CS uses a compute pipeline. On both revisions, all 2,319 emitted CS
passed validation and 16 failed pipeline creation. Every emitted VS, PS
and MS produced a pipeline, including the 71 MS that still failed
validation at `7b63628`.

## Origin of the 24,275 validations

The historical file `D:\xtp-data\w6-before.json` corresponds to
`37ee7ea`. Its global output fingerprint
(`9255acc313a2b4dd337d2bceef409118d5b0753c09e3568bbbd9e2358a305b06`),
the pack, the 24,433 translations and the 158 refusals match the survey
recorded in Hangar task `7452518e`.

The 24,275 programs of this reference stay accepted at `2545065`. Between
`37ee7ea` and `2545065`, the fingerprint of 824 of them had already changed
(558 CS, 258 PS, 8 MS) and 978 additional programs had been
accepted. The comparison baseline for the fix is therefore **25,253**
validations.

## Heap access in two programs of the pack

Both programs were translated at `7b63628`, for 32–32 lanes.
The operations below are the calls in the module disassembled with
`dxc -dumpbin`. Both validators received the same emitted bytes.

| Program (`record/program`) | Operations and resource | DXIL ShaderFlags | SFI0 | dxil.dll 1.6 (PC) | dxil.dll 1.9 (L4) |
|---|---|---:|---:|---|---|
| Skinning VS `0x54c00/0` | `createHandleFromHeap` then `annotateHandle` as an `RWStructuredBuffer` with stride 12. Three `rawBufferStore.i32` on this handle, at offsets 0, 4 and 8. | `0x40100010` | `0x02008000` | PSV0 refusal | Accepted |
| Skinned-mesh shadow MS `0x49fec/0` | Nine `createHandleFromHeap`. SRV reads through `rawBufferLoad` and `bufferLoad`. The `storeVertexOutput` and `emitIndices` writes target geometry. | `0x40000010` | `0x02000000` | Accepted | Accepted |

Of the 9,181 MS translated at `7b63628`, the 4,653 distinct fingerprints
were reproduced on the L4. Their modules contain zero buffer or texture
store calls, UAV atomics or UAV counter updates. The VS provides the
positive case of a write through the heap.

The VS writes to a heap UAV with ShaderFlags bit 16 and SFI0 bit
`0x4` both at zero. With SDK 22621, the 1.6 validator
reports only `Pipeline State Validation`. Its assembler rebuilds a
288-byte PSV0 part (272 in the translation). Replacing only this
part makes 1.6 accept the VS, with identical DXIL and SFI0.
The control reassembled with the original PSV0 keeps the refusal. The
previous task had also observed the PSV0 diagnostic in 1.6 for three
pre-existing tests and for the 35 outputs from rotation 0 onward.

## Check of the synthetic signatures

On the L4, the survey for the sorting fix `2d41fc1` is byte-for-byte
identical to the one for its first parent `a012d2c`. Both JSON files
have the SHA-256 `32cbc24159467d1091ce0b8f820606cbada02565ed26af02397e7e82eba93c4b`.
The 25,768 translations, 25,697 of them accepted, keep their fingerprints
and verdicts.

The 11 tests of the `validator` filter pass on the Windows farm.
`IDxcVersionInfo::GetVersion` returns 1.9 for the x64 DLL of SDK 26100.
In the 35-element signatures, rotations 15 and 16 give
the indices [0, 1] and [1, 0] respectively for the two clipping
distances in ISG1 and OSG1. The validator accepts both orders in
their respective containers.

For PSG1, the control mesh with a single `SV_PrimitiveID` passes. Adding
a second one in the same register makes it possible to test the permutation
of same-key rows in the emitted container. The test checks both orders and
the expected refusal of the repeated semantic.
