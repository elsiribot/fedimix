//! Command-line flags. Parsed before GApplication sees any arguments.

use std::path::{Path, PathBuf};

use fedimix_core::{DemoScenario, Network};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartPage {
    Saving,
    Spending,
    Coins,
    CoinDetail,
    Moving,
    Notes,
    SendReview,
    Send,
    Receive,
    Refill,
    RefillExpanded,
    Join,
    SetupSaving,
    Settings,
    About,
}

impl StartPage {
    fn from_name(s: &str) -> Option<Self> {
        Some(match s {
            "saving" => Self::Saving,
            "spending" => Self::Spending,
            "coins" => Self::Coins,
            "coin-detail" => Self::CoinDetail,
            "moving" => Self::Moving,
            "notes" => Self::Notes,
            "send-review" => Self::SendReview,
            "send" => Self::Send,
            "receive" => Self::Receive,
            "refill" => Self::Refill,
            "refill-expanded" => Self::RefillExpanded,
            "join" => Self::Join,
            "setup-saving" => Self::SetupSaving,
            "settings" => Self::Settings,
            "about" => Self::About,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub demo: bool,
    pub scenario: DemoScenario,
    pub page: Option<StartPage>,
    /// Script for the in-app demo driver.
    pub drive: Option<std::path::PathBuf>,
    /// Milliseconds the demo spends in each operation state, in place of its
    /// own, so a flow's screenshot cannot outlast a state.
    pub demo_step_ms: Option<u64>,
    /// `None` picks the network's default folder.
    pub data_dir: Option<PathBuf>,
    pub network: Network,
}

/// Where `network` keeps its data unless `--data-dir` says otherwise.
pub fn default_data_dir(
    network: Network,
    xdg_data_home: Option<&Path>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    let base = match xdg_data_home.filter(|path| path.is_absolute()) {
        Some(path) => path.to_path_buf(),
        None => home?.join(".local/share"),
    };
    Some(base.join(match network {
        Network::Bitcoin => "fedimix",
        Network::Signet => "fedimix-signet",
        Network::Regtest => "fedimix-regtest",
    }))
}

pub fn parse(args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut args = args;
    let mut demo = false;
    let mut scenario = None;
    let mut page = None;
    let mut drive = None;
    let mut demo_step_ms = None;
    let mut data_dir = None;
    let mut network = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--demo" => demo = true,
            "--demo-scenario" => {
                let v = args.next().ok_or("--demo-scenario needs a value")?;
                scenario = Some(v.parse::<DemoScenario>()?);
            }
            "--demo-page" => {
                let v = args.next().ok_or("--demo-page needs a value")?;
                page = Some(StartPage::from_name(&v).ok_or_else(|| format!("unknown page '{v}'"))?);
            }
            "--demo-drive" => {
                let v = args.next().ok_or("--demo-drive needs a file")?;
                drive = Some(std::path::PathBuf::from(v));
            }
            "--demo-step-ms" => {
                let v = args.next().ok_or("--demo-step-ms needs milliseconds")?;
                demo_step_ms = Some(
                    v.parse::<u64>()
                        .map_err(|_| format!("--demo-step-ms needs milliseconds, got '{v}'"))?,
                );
            }
            "--data-dir" => {
                let v = args.next().ok_or("--data-dir needs a folder")?;
                data_dir = Some(PathBuf::from(v));
            }
            "--network" => {
                let v = args.next().ok_or("--network needs a value")?;
                network = Some(v.parse::<Network>()?);
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    if !demo && (scenario.is_some() || page.is_some() || demo_step_ms.is_some()) {
        return Err("--demo-scenario, --demo-page and --demo-step-ms require --demo".to_string());
    }
    if demo && (data_dir.is_some() || network.is_some()) {
        return Err("--data-dir and --network do not apply to --demo".to_string());
    }
    // A script can click the money UI, so it never runs on real bitcoin.
    if !demo && drive.is_some() && network.is_none_or(|n| n == Network::Bitcoin) {
        return Err("--demo-drive is refused on --network bitcoin".to_string());
    }
    Ok(Args {
        demo,
        scenario: scenario.unwrap_or(DemoScenario::Funded),
        page,
        drive,
        demo_step_ms,
        data_dir,
        network: network.unwrap_or(Network::Bitcoin),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Args, String> {
        parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn demo_step_ms_takes_milliseconds_and_needs_demo() {
        assert_eq!(
            p(&["--demo", "--demo-step-ms", "2500"])
                .unwrap()
                .demo_step_ms,
            Some(2500)
        );
        assert!(p(&["--demo", "--demo-step-ms", "slow"]).is_err());
        assert!(p(&["--demo-step-ms", "10"]).is_err());
    }

    #[test]
    fn parses_demo() {
        let a = p(&["--demo"]).unwrap();
        assert!(a.demo);
        assert_eq!(a.scenario, DemoScenario::Funded);
        assert_eq!(a.page, None);
    }

    #[test]
    fn parses_scenario_and_page() {
        let a = p(&[
            "--demo",
            "--demo-scenario",
            "below-target",
            "--demo-page",
            "coin-detail",
        ])
        .unwrap();
        assert_eq!(a.scenario, DemoScenario::BelowTarget);
        assert_eq!(a.page, Some(StartPage::CoinDetail));
    }

    #[test]
    fn rejects_unknown_flag() {
        assert!(p(&["--demo", "--bogus"]).is_err());
        assert!(p(&["--demo", "--demo-page", "nope"]).is_err());
        assert!(p(&["--demo", "--demo-scenario"]).is_err());
    }

    #[test]
    fn rejects_scenario_without_demo() {
        assert!(p(&["--demo-scenario", "funded"]).is_err());
        assert!(p(&["--demo-page", "saving"]).is_err());
    }

    #[test]
    fn parses_drive_file_with_or_without_demo() {
        let a = p(&["--demo", "--demo-drive", "flow.drive"]).unwrap();
        assert_eq!(a.drive, Some("flow.drive".into()));
        let a = p(&["--network", "regtest", "--demo-drive", "flow.drive"]).unwrap();
        assert!(!a.demo);
        assert_eq!(a.drive, Some("flow.drive".into()));
        assert!(p(&["--demo", "--demo-drive"]).is_err());
    }

    /// A script can click the money UI, so it never runs on real bitcoin.
    #[test]
    fn a_drive_is_refused_on_bitcoin() {
        assert!(p(&["--demo-drive", "flow.drive"]).is_err());
        assert!(p(&["--network", "bitcoin", "--demo-drive", "flow.drive"]).is_err());
        assert!(p(&["--network", "signet", "--demo-drive", "flow.drive"]).is_ok());
    }

    #[test]
    fn real_is_the_default() {
        let a = p(&[]).unwrap();
        assert!(!a.demo);
        assert_eq!(a.network, Network::Bitcoin);
        assert_eq!(a.data_dir, None);
    }

    #[test]
    fn data_dir_and_network_parse() {
        let a = p(&["--data-dir", "/tmp/wallet", "--network", "regtest"]).unwrap();
        assert_eq!(a.data_dir, Some("/tmp/wallet".into()));
        assert_eq!(a.network, Network::Regtest);
        assert_eq!(
            p(&["--network", "signet"]).unwrap().network,
            Network::Signet
        );
        assert!(p(&["--network", "testnet"]).is_err());
        assert!(p(&["--network"]).is_err());
        assert!(p(&["--data-dir"]).is_err());
    }

    #[test]
    fn real_flags_do_nothing_in_the_demo_so_they_are_refused() {
        assert!(p(&["--demo", "--network", "regtest"]).is_err());
        assert!(p(&["--demo", "--data-dir", "/tmp/wallet"]).is_err());
    }

    #[test]
    fn test_networks_never_share_a_folder() {
        let xdg = Some(Path::new("/xdg"));
        let home = Some(Path::new("/home/u"));
        let dir = default_data_dir;
        assert_eq!(
            dir(Network::Bitcoin, xdg, home),
            Some("/xdg/fedimix".into())
        );
        assert_eq!(
            dir(Network::Signet, xdg, home),
            Some("/xdg/fedimix-signet".into())
        );
        assert_eq!(
            dir(Network::Regtest, xdg, home),
            Some("/xdg/fedimix-regtest".into())
        );
        assert_eq!(
            dir(Network::Bitcoin, None, home),
            Some("/home/u/.local/share/fedimix".into())
        );
        // The XDG spec ignores a relative path, which an empty value is too.
        for ignored in ["", "relative/path"] {
            assert_eq!(
                dir(Network::Regtest, Some(Path::new(ignored)), home),
                Some("/home/u/.local/share/fedimix-regtest".into())
            );
        }
        assert_eq!(dir(Network::Bitcoin, None, None), None);
    }

    #[test]
    fn no_args_is_not_demo() {
        assert!(!p(&[]).unwrap().demo);
    }
}
