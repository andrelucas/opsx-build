use anyhow::Result;
use opsx_build::{app::App, cli::Cli, ui::TerminalUi};

fn main() -> Result<()> {
    let cli = Cli::load()?;
    let workflow_dashboard =
        !cli.interactive && cli.test_connection.is_none() && cli.request != "configure";
    let ui = TerminalUi::new(cli.verbose, cli.debug, workflow_dashboard);
    App::new(cli, ui).run()
}
