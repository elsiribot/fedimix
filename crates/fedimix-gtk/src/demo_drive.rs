//! In-app demo driver: `--demo-drive <file>` replays scripted UI steps.
//!
//! OS-level input injection does not reach GTK under headless sway, so the
//! driver walks the widget tree itself and emits clicks directly.
//!
//! Script syntax, one step per line (`#` starts a comment):
//!
//! - `click <label>`: activate the sensitive, mapped button, check button,
//!   switch row or activatable row whose label (or a child label) equals
//!   `<label>`; open dialogs are searched first. Polls up to 3 s.
//! - `type <title> <text>` (title optionally in double quotes): set an entry
//!   or spin row by title.
//! - `expect <text>`: wait up to 3 s for a mapped label containing `<text>`.
//! - `wait <ms>`, `shot <name>` (prints `SHOT <name>`, then waits for a line
//!   on stdin, or 500 ms when stdin is a terminal or closed).
//! - `respond <id>`: answer the visible AlertDialog with response `<id>`.
//! - `action <name>`: activate an application action such as `app.settings`
//!   (menu popovers are not in the widget tree until opened).
//!
//! A failing step prints `DRIVE FAILED: <step>` and exits 1; a finished
//! script prints `DRIVE OK` and quits the application.

use std::fmt;
use std::io::{IsTerminal, Write};
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;

const POLL: Duration = Duration::from_millis(100);
const POLL_LIMIT: u32 = 30;
const SHOT_PAUSE: Duration = Duration::from_millis(500);
const SHOT_LINE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Click(String),
    Type { widget: String, text: String },
    Expect(String),
    Wait(u64),
    Shot(String),
    Respond(String),
    Action(String),
}

pub fn parse(src: &str) -> Result<Vec<Step>, String> {
    let mut steps = Vec::new();
    for (i, raw) in src.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let step = parse_line(line).map_err(|e| format!("line {}: {e}", i + 1))?;
        steps.push(step);
    }
    Ok(steps)
}

fn parse_line(line: &str) -> Result<Step, String> {
    let (cmd, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let rest = rest.trim();
    let need = |what: &str| {
        if rest.is_empty() {
            Err(format!("'{cmd}' needs {what}"))
        } else {
            Ok(rest.to_string())
        }
    };
    Ok(match cmd {
        "click" => Step::Click(need("a label")?),
        "expect" => Step::Expect(need("text")?),
        "shot" => Step::Shot(need("a name")?),
        "respond" => Step::Respond(need("a response id")?),
        "action" => Step::Action(need("an action name")?),
        "wait" => Step::Wait(
            rest.parse()
                .map_err(|_| format!("'wait' needs milliseconds, got '{rest}'"))?,
        ),
        "type" => {
            let (widget, text) = if let Some(quoted) = rest.strip_prefix('"') {
                quoted
                    .split_once('"')
                    .ok_or("unterminated quote in widget title")?
            } else {
                rest.split_once(char::is_whitespace)
                    .ok_or("'type' needs a widget title and text")?
            };
            let text = text.trim();
            if widget.is_empty() || text.is_empty() {
                return Err("'type' needs a widget title and text".to_string());
            }
            Step::Type {
                widget: widget.to_string(),
                text: text.to_string(),
            }
        }
        other => return Err(format!("unknown step '{other}'")),
    })
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Step::Click(l) => write!(f, "click {l}"),
            Step::Type { widget, text } => write!(f, "type \"{widget}\" {text}"),
            Step::Expect(t) => write!(f, "expect {t}"),
            Step::Wait(ms) => write!(f, "wait {ms}"),
            Step::Shot(n) => write!(f, "shot {n}"),
            Step::Respond(id) => write!(f, "respond {id}"),
            Step::Action(a) => write!(f, "action {a}"),
        }
    }
}

/// Everything the driver needs from the running application.
pub struct Env {
    pub app: adw::Application,
    pub window: adw::ApplicationWindow,
    pub stack: adw::ViewStack,
}

/// Run `steps` on the GTK main loop, then quit (or exit 1 on failure).
pub fn start(env: Env, steps: Vec<Step>) {
    glib::spawn_future_local(async move {
        for step in &steps {
            if let Err(why) = run_step(&env, step).await {
                println!("DRIVE FAILED: {step} ({why})");
                let _ = std::io::stdout().flush();
                std::process::exit(1);
            }
        }
        println!("DRIVE OK");
        let _ = std::io::stdout().flush();
        env.app.quit();
    });
}

async fn run_step(env: &Env, step: &Step) -> Result<(), String> {
    match step {
        Step::Click(label) => {
            poll(|| click(env, label).then_some(()))
                .await
                .ok_or_else(|| format!("no sensitive visible control '{label}'"))?;
            // Let the click's async consequences (core calls, dialogs) settle.
            glib::timeout_future(Duration::from_millis(150)).await;
        }
        Step::Type { widget, text } => {
            poll(|| type_into(env, widget, text).then_some(()))
                .await
                .ok_or_else(|| format!("no entry titled '{widget}'"))?;
            glib::timeout_future(Duration::from_millis(150)).await;
        }
        Step::Expect(text) => {
            poll(|| has_label_containing(env, text).then_some(()))
                .await
                .ok_or_else(|| format!("no visible label containing '{text}'"))?;
        }
        Step::Wait(ms) => glib::timeout_future(Duration::from_millis(*ms)).await,
        Step::Shot(name) => shot(name).await,
        Step::Respond(id) => {
            let dialog = env
                .window
                .visible_dialog()
                .and_then(|d| d.downcast::<adw::AlertDialog>().ok())
                .ok_or("no visible AlertDialog")?;
            if !dialog.has_response(id) {
                return Err(format!("dialog has no response '{id}'"));
            }
            dialog.emit_by_name::<()>("response", &[id]);
            dialog.force_close();
            glib::timeout_future(Duration::from_millis(150)).await;
        }
        Step::Action(name) => {
            let (_, action) = name.split_once('.').ok_or("action needs app.<name>")?;
            if !env.app.has_action(action) {
                return Err(format!("no application action '{action}'"));
            }
            env.app.activate_action(action, None);
            glib::timeout_future(Duration::from_millis(150)).await;
        }
    }
    Ok(())
}

async fn poll<T>(mut f: impl FnMut() -> Option<T>) -> Option<T> {
    for _ in 0..POLL_LIMIT {
        if let Some(v) = f() {
            return Some(v);
        }
        glib::timeout_future(POLL).await;
    }
    f()
}

async fn shot(name: &str) {
    println!("SHOT {name}");
    let _ = std::io::stdout().flush();
    if std::io::stdin().is_terminal() {
        glib::timeout_future(SHOT_PAUSE).await;
        return;
    }
    // A thread blocks on stdin; the main loop keeps running meanwhile.
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let mut line = String::new();
        let got = std::io::stdin().read_line(&mut line).unwrap_or(0);
        let _ = tx.send_blocking(got > 0);
    });
    let line = async { rx.recv().await.unwrap_or(false) };
    let timeout = async {
        glib::timeout_future(SHOT_LINE_TIMEOUT).await;
        false
    };
    let got_line = futures_lite_or(line, timeout).await;
    if !got_line {
        glib::timeout_future(SHOT_PAUSE).await;
    }
}

/// Resolve with whichever future finishes first.
async fn futures_lite_or(
    a: impl std::future::Future<Output = bool>,
    b: impl std::future::Future<Output = bool>,
) -> bool {
    use std::pin::pin;
    use std::task::Poll;
    let (mut a, mut b) = (pin!(a), pin!(b));
    std::future::poll_fn(move |cx| {
        if let Poll::Ready(v) = a.as_mut().poll(cx) {
            return Poll::Ready(v);
        }
        b.as_mut().poll(cx)
    })
    .await
}

/// Depth-first, pre-order list of `root` and everything below it.
fn descendants(root: &gtk::Widget) -> Vec<gtk::Widget> {
    let mut out = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(w) = stack.pop() {
        out.push(w.clone());
        let mut children = Vec::new();
        let mut child = w.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            children.push(c);
        }
        stack.extend(children.into_iter().rev());
    }
    out
}

/// Search roots: the open dialog first, then the window.
fn roots(env: &Env) -> Vec<gtk::Widget> {
    let mut roots = Vec::new();
    if let Some(d) = env.window.visible_dialog() {
        roots.push(d.upcast::<gtk::Widget>());
    }
    roots.push(env.window.clone().upcast());
    roots
}

fn live(w: &gtk::Widget) -> bool {
    w.is_mapped() && w.is_sensitive()
}

fn label_texts(w: &gtk::Widget) -> Vec<String> {
    descendants(w)
        .iter()
        .filter_map(|d| d.downcast_ref::<gtk::Label>())
        .filter(|l| l.is_mapped())
        .map(|l| l.text().to_string())
        .collect()
}

fn has_label_containing(env: &Env, text: &str) -> bool {
    roots(env).iter().any(|r| {
        descendants(r).iter().any(|d| {
            d.downcast_ref::<gtk::Label>()
                .is_some_and(|l| l.is_mapped() && l.text().contains(text))
        })
    })
}

fn title_matches(w: &gtk::Widget, label: &str) -> bool {
    w.downcast_ref::<adw::PreferencesRow>()
        .is_some_and(|r| r.title() == label)
        || label_texts(w).iter().any(|t| t == label)
}

/// Activate the first matching control. Controls win over bare rows so a
/// button inside an activatable row is not shadowed by the row.
fn click(env: &Env, label: &str) -> bool {
    for root in roots(env) {
        let all = descendants(&root);
        for w in all.iter().filter(|w| live(w)) {
            if let Some(b) = w.downcast_ref::<gtk::CheckButton>() {
                if b.label().is_some_and(|l| l == label) {
                    b.set_active(!b.is_active());
                    return true;
                }
            } else if let Some(row) = w.downcast_ref::<adw::SwitchRow>() {
                if row.title() == label {
                    row.set_active(!row.is_active());
                    return true;
                }
            } else if let Some(b) = w.downcast_ref::<gtk::Button>() {
                let own = b.label().is_some_and(|l| l == label)
                    || b.tooltip_text().is_some_and(|t| t == label);
                if own || label_texts(w).iter().any(|t| t == label) {
                    b.emit_clicked();
                    return true;
                }
            }
        }
        for w in all.iter().filter(|w| live(w)) {
            if let Some(row) = w.downcast_ref::<gtk::ListBoxRow>()
                && row.is_activatable()
                && title_matches(w, label)
            {
                row.activate();
                return true;
            }
        }
    }
    // The header bar's back button is internal: pop the deepest pushed page.
    if label == "Back" {
        for root in roots(env) {
            let pushed = descendants(&root)
                .into_iter()
                .filter_map(|w| w.downcast::<adw::NavigationView>().ok())
                .filter(|n| n.is_mapped() && n.navigation_stack().n_items() > 1)
                .last();
            if let Some(nav) = pushed {
                return nav.pop();
            }
        }
    }
    // Item menus: open a MenuButton by its tooltip, then click its entries
    // (GtkModelButton is private API, so it is matched by type name).
    for root in roots(env) {
        for w in descendants(&root).iter().filter(|w| live(w)) {
            if w.type_().name() == "GtkModelButton" && label_texts(w).iter().any(|t| t == label) {
                w.activate();
                return true;
            }
            if let Some(mb) = w.downcast_ref::<gtk::MenuButton>()
                && mb.tooltip_text().is_some_and(|t| t == label)
            {
                mb.popup();
                return true;
            }
        }
    }
    // Tab switchers are internal widgets; fall back to the page titles.
    if let Some(page) = env
        .stack
        .pages()
        .iter::<adw::ViewStackPage>()
        .flatten()
        .find(|p| p.title().is_some_and(|t| t == label))
        && env.stack.is_mapped()
        && let Some(child) = page.child().into()
    {
        env.stack.set_visible_child(&child);
        return true;
    }
    false
}

fn type_into(env: &Env, title: &str, text: &str) -> bool {
    for root in roots(env) {
        for w in descendants(&root).iter().filter(|w| live(w)) {
            if let Some(row) = w.downcast_ref::<adw::EntryRow>()
                && row.title() == title
            {
                row.set_text(text);
                return true;
            }
            if let Some(row) = w.downcast_ref::<adw::SpinRow>()
                && row.title() == title
                && let Ok(v) = text.parse::<f64>()
            {
                row.set_value(v);
                return true;
            }
            if let Some(e) = w.downcast_ref::<gtk::Entry>()
                && e.placeholder_text().is_some_and(|p| p == title)
            {
                e.set_text(text);
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_step_kinds() {
        let src = "\
# a comment
click Exit now (lower privacy)

type \"Amount (BTC)\" 0.01
type Target 0.1 extra words
expect Fedi Alpha
wait 250
shot funded-moving
respond exit
action app.settings
";
        assert_eq!(
            parse(src).unwrap(),
            vec![
                Step::Click("Exit now (lower privacy)".into()),
                Step::Type {
                    widget: "Amount (BTC)".into(),
                    text: "0.01".into()
                },
                Step::Type {
                    widget: "Target".into(),
                    text: "0.1 extra words".into()
                },
                Step::Expect("Fedi Alpha".into()),
                Step::Wait(250),
                Step::Shot("funded-moving".into()),
                Step::Respond("exit".into()),
                Step::Action("app.settings".into()),
            ]
        );
    }

    #[test]
    fn rejects_bad_steps_with_line_number() {
        for bad in [
            "bogus x",
            "click",
            "wait soon",
            "type onlywidget",
            "shot",
            "type \"open",
        ] {
            let err = parse(bad).unwrap_err();
            assert!(err.contains("line 1"), "{bad}: {err}");
        }
        assert!(parse("click ok\nwait x").unwrap_err().contains("line 2"));
    }
}
