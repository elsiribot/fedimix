//! Command-line flags. Parsed before GApplication sees any arguments.

use fedimix_core::DemoScenario;

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
}

pub fn parse(args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut args = args;
    let mut demo = false;
    let mut scenario = None;
    let mut page = None;
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
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    if !demo && (scenario.is_some() || page.is_some()) {
        return Err("--demo-scenario and --demo-page require --demo".to_string());
    }
    Ok(Args {
        demo,
        scenario: scenario.unwrap_or(DemoScenario::Funded),
        page,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Args, String> {
        parse(args.iter().map(|s| s.to_string()))
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
    fn no_args_is_not_demo() {
        assert!(!p(&[]).unwrap().demo);
    }
}
