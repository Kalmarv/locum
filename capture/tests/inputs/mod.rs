//! The real-data inputs this crate's tests read, each with what it is, where it comes from and whether a full run
//! needs it (`primase_testdata::inputs!`; tests/inputs/README.md lists them). Integration tests reach it with
//! `mod inputs;`, and tests/real_data_inputs.rs checks that every query the crate's sources write is declared here.

primase_testdata::inputs! {
    games {
        General = "general",
    }
    installs {}
    keys {}
    tools {}
    oracle_sets {
        Corpus = "corpus" {
            about: "files as their makers stored them: shader programs gathered from games, test suites and SDKs",
            source: "the user's own installs and outside projects' published files, kept outside the repository",
        },
    }
    declared {
        Query::Oracle(OracleSet::Corpus, Game::General, "slim") => {
            status: Live,
            gate: true,
            about: r#"the slim shader set: 8,287 programs (`programs/<fam>/<sha256>.<fam>`; 3,671 DXBC, 4,059 DXIL, 557 Direct3D 9), each beside Microsoft's listing, chosen to reach every llvm-cov region and branch side of dx and iridia that the full 108,394-program pool reaches; tests/slim.rs"#,
            source: r#"primase-oracles `producers/slim-set` over the oracle home's game corpora and disassembler sets and outside projects' test corpora (recorded since primase-testdata 0.1.17, its slim.tsv as of 0.2.2, held as slim.zip from 0.2.6); programs stay outside the repository"#,
        },
    }
    debug {}
}
