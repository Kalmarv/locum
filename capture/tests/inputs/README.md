# Real-data inputs

What this crate's real-data test (`tests/slim.rs`) reads, as tests/inputs/mod.rs declares it: the keys of the
configuration file `GAME_DATA` names (its layout is described by
[primase-testdata](https://github.com/Kalmarv/primase-testdata)), the oracle sets and the files of each. Where its
input is not configured the test skips with a line naming its key; with `[run] require = 'gate'` it fails instead.

<!-- real-data inputs: made by primase_testdata::doc::tables from tests/inputs/mod.rs -->

#### Configuration keys

| key | gate | about | source |
| --- | --- | --- | --- |
| `scratch.root` | - | where real-data tests write their outputs (the OS temporary folder when unset) | - |

#### Oracle sets (`<oracles.root>/<set>/<game>/`)

| set | about | source |
| --- | --- | --- |
| `oracles.corpus` | files as their makers stored them: shader programs gathered from games, test suites and SDKs | the user's own installs and outside projects' published files, kept outside the repository |

#### Declared files

| key | status | gate | about | source |
| --- | --- | --- | --- | --- |
| `oracles.corpus.general/slim` | live | yes | the slim shader set: 8,287 programs (`programs/<fam>/<sha256>.<fam>`; 3,671 DXBC, 4,059 DXIL, 557 Direct3D 9), each beside Microsoft's listing, chosen to reach every llvm-cov region and branch side of dx and iridia that the full 108,394-program pool reaches; tests/slim.rs | primase-oracles `producers/slim-set` over the oracle home's game corpora and disassembler sets and outside projects' test corpora (recorded since primase-testdata 0.1.17, its slim.tsv as of 0.2.2); programs stay outside the repository |

<!-- end of real-data inputs -->
