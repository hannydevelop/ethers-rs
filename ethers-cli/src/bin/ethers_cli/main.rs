//! Main entry point for EthersCli

#![deny(warnings, missing_docs, trivial_casts, unused_qualifications)]
#![forbid(unsafe_code)]

use ethers_cli::application::APP;

/// Boot EthersCli
fn main() {
    abscissa_core::boot(&APP);
}
