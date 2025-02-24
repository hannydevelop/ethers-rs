mod ganache;
mod infura;

use abscissa_core::{Clap, Command, Runnable};

/// send ETH commands for ethers-cli
#[derive(Command, Debug, Clap, Runnable)]
pub enum SendETHCmd {
    Ganache(ganache::GanacheCmd),

    #[clap(subcommand)]
    Infura(infura::InfuraCmd),
}
