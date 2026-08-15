use anyhow::Result;
use ospx_build::{app::App, cli::Cli, ui::TerminalUi};

fn main() -> Result<()> {
    let cli = Cli::load()?;
    let ui = TerminalUi::new(cli.verbose, cli.debug);
    App::new(cli, ui).run()
}
