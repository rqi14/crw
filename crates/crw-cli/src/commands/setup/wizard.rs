//! Main setup wizard flow.

use crate::commands::setup::ui::SetupError;
use crate::commands::setup::{cloud, local, ui};
use dialoguer::Select;

/// Setup mode choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupMode {
    Cloud,
    Local,
}

/// Run the interactive setup wizard.
pub async fn run_wizard() -> Result<(), SetupError> {
    ui::print_header();

    let mode = select_setup_mode()?;

    match mode {
        SetupMode::Cloud => cloud::run().await,
        SetupMode::Local => local::run(false).await,
    }
}

/// Prompt user to select Cloud or Local setup.
fn select_setup_mode() -> Result<SetupMode, SetupError> {
    println!("  How do you want to run CRW?  (you can switch anytime with `crw setup`)");
    println!();

    let items = vec![
        "☁️  Cloud — ready in 30 seconds                      ⭐ Recommended\n        • 500 free credits — no card, nothing to pay\n        • No Docker, nothing to run — works instantly\n        • Managed & always up to date\n        • Sign up with GitHub/Google, paste your key, done",
        "🏠 Local — self-hosted, unlimited & free\n        • Basic scraping works immediately\n        • Add a browser only for JavaScript-heavy sites\n        • Docker is needed only if you want local web search",
    ];

    let selection = Select::with_theme(&ui::select_style())
        .items(&items)
        .default(0)
        .interact_opt()
        .map_err(ui::handle_dialoguer_error)?
        .ok_or(SetupError::Cancelled)?;

    Ok(match selection {
        0 => SetupMode::Cloud,
        1 => SetupMode::Local,
        _ => unreachable!(),
    })
}
