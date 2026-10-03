//! Print the Baryon CRDs as YAML. `hack/verify-crds.sh` compares the output
//! with `deploy/base/crds/`.

use std::io::Write;
use std::process::ExitCode;

fn main() -> ExitCode {
    match baryon_api::crds_yaml() {
        Ok(yaml) => {
            let mut out = std::io::stdout().lock();
            if out.write_all(yaml.as_bytes()).is_err() {
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("rendering CRDs: {e}");
            ExitCode::FAILURE
        }
    }
}
