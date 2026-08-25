//! Write `grammar/vocabulary.txt` out of the compiler that defines it.
//!
//! Run after changing the language's words or punctuation:
//!
//! ```bash
//! cargo run --bin export-vocabulary
//! ```
//!
//! It writes the file rather than printing it, so that there is no redirection
//! to get wrong and no shell to get it wrong differently on. `cargo test` is
//! what notices that it needed running — see [`tinyc::vocabulary`].
//!
//! It sits in `tools/` rather than in `src/bin/` on purpose: see the comment
//! beside it in `Cargo.toml`.

use std::path::Path;
use std::process::ExitCode;

use tinyc::vocabulary;

fn main() -> ExitCode {
    // Relative to the manifest rather than to wherever this was run from: the
    // file belongs to the repository, not to a working directory.
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(vocabulary::PATH);

    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("error: cannot create {}: {e}", parent.display());
        return ExitCode::FAILURE;
    }

    match std::fs::write(&path, vocabulary::export()) {
        Ok(()) => {
            println!("wrote {}", path.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: cannot write {}: {e}", path.display());
            ExitCode::FAILURE
        }
    }
}
