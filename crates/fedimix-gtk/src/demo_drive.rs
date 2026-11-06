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
//! - `click-containing <text>`: activate the sensitive, mapped button whose
//!   label contains `<text>`, for a label holding an amount the script cannot
//!   know, such as one net of a federation's fee. Polls up to 3 s.
//! - `type <title> <text>` (title optionally in double quotes): set an entry
//!   or spin row by title.
//! - `expect <text>`: wait up to 3 s for a mapped label or entry containing
//!   `<text>`.
//! - `type-env <title> <VAR>` (title quoted as for `type`): as `type`, with the
//!   value of the environment variable `VAR`.
//! - `save-text <text> <path>` (text quoted as for `type`): write the whole text
//!   of the mapped label containing `<text>` to `<path>`, relative to the
//!   working directory. Polls up to 3 s.
//! - `expect-slow <text>`: as `expect`, for a real federation: up to 60 s.
//! - `expect-within <seconds> <text>`: as `expect`, for up to `<seconds>`, for
//!   what waits on blocks and a federation's poll.
//! - `expect-gone-within <seconds> <text>`: as `expect-not`, for up to
//!   `<seconds>`.
//! - `expect-numbered <n>`: the mapped labels hold exactly the words `1. x` to
//!   `<n>. x`, each one word.
//! - `expect-clipboard <text>`: the clipboard holds `<text>`.
//! - `expect-clipboard-empty`: the clipboard holds no text.
//! - `wait <ms>`, `shot <name>` (prints `SHOT <name>`, then waits for a line
//!   on stdin, or 500 ms when stdin is a terminal or closed).
//! - `expect-not <text>`: wait up to 3 s until no mapped label contains `<text>`.
//! - `expect-absent <text>`: no label holds `<text>`, even on a hidden page.
//! - `expect-pinned`: the page on top of the window cannot be popped.
//! - `apply <title>`: press the apply button of an entry row.
//! - `expect-locked`: the open dialog cannot be closed and its page cannot be
//!   popped, which is what Escape and Back go through.
//! - `scroll-end`: scroll the visible scrolled windows to their bottom.
//! - `activate`: activate the application again, as a second launch does.
//! - `expect-windows <n>`: the application has `<n>` windows.
//! - `close-dialog`: close the open dialog, as Escape does.
//! - `expect-disabled <text>` / `expect-enabled <text>`: the list row showing
//!   `<text>` has a menu button or button, and none (or all) of them can be used.
//! - `action <name>`: activate an application action such as `app.settings`
//!   (menu popovers are not in the widget tree until opened).
//!
//! A failing step prints `DRIVE FAILED: <step>`, and a finished script prints
//! `DRIVE OK`. Both quit through the `quit` action, so a real wallet shuts down
//! cleanly; after a failure the process exits 1 (see [`failed`]).

use std::fmt;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;

const POLL: Duration = Duration::from_millis(100);
const POLL_LIMIT: u32 = 30;
/// `expect-slow`: at this interval, 60 s in all.
const SLOW_POLL_LIMIT: u32 = 600;
const SHOT_PAUSE: Duration = Duration::from_millis(500);
const SHOT_LINE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Click(String),
    ClickContaining(String),
    Type { widget: String, text: String },
    TypeEnv { widget: String, var: String },
    SaveText { text: String, path: String },
    Expect(String),
    ExpectSlow(String),
    ExpectWithin { secs: u64, text: String },
    ExpectGoneWithin { secs: u64, text: String },
    ExpectClipboard(String),
    ExpectClipboardEmpty,
    ExpectNumbered(u32),
    ExpectNot(String),
    ExpectAbsent(String),
    ExpectLocked,
    ExpectPinned,
    ExpectRowControls { text: String, enabled: bool },
    Apply(String),
    CloseDialog,
    ScrollEnd,
    Activate,
    ExpectWindows(usize),
    Wait(u64),
    Shot(String),
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
        "click-containing" => Step::ClickContaining(need("text")?),
        "expect" => Step::Expect(need("text")?),
        "expect-slow" => Step::ExpectSlow(need("text")?),
        "expect-within" => {
            let (secs, text) = seconds_and_text(cmd, rest)?;
            Step::ExpectWithin { secs, text }
        }
        "expect-gone-within" => {
            let (secs, text) = seconds_and_text(cmd, rest)?;
            Step::ExpectGoneWithin { secs, text }
        }
        "expect-clipboard" => Step::ExpectClipboard(need("text")?),
        "expect-clipboard-empty" => Step::ExpectClipboardEmpty,
        "expect-numbered" => Step::ExpectNumbered(
            rest.parse()
                .map_err(|_| format!("'expect-numbered' needs a count, got '{rest}'"))?,
        ),
        "expect-not" => Step::ExpectNot(need("text")?),
        "expect-absent" => Step::ExpectAbsent(need("text")?),
        "expect-disabled" => Step::ExpectRowControls {
            text: need("row text")?,
            enabled: false,
        },
        "expect-enabled" => Step::ExpectRowControls {
            text: need("row text")?,
            enabled: true,
        },
        "expect-locked" => Step::ExpectLocked,
        "expect-pinned" => Step::ExpectPinned,
        "apply" => Step::Apply(need("a title")?),
        "close-dialog" => Step::CloseDialog,
        "scroll-end" => Step::ScrollEnd,
        "activate" => Step::Activate,
        "expect-windows" => Step::ExpectWindows(
            rest.parse()
                .map_err(|_| format!("'expect-windows' needs a count, got '{rest}'"))?,
        ),
        "shot" => Step::Shot(need("a name")?),
        "action" => Step::Action(need("an action name")?),
        "wait" => Step::Wait(
            rest.parse()
                .map_err(|_| format!("'wait' needs milliseconds, got '{rest}'"))?,
        ),
        "type" => {
            let (widget, text) = two_args(cmd, rest)?;
            Step::Type { widget, text }
        }
        "type-env" => {
            let (widget, var) = two_args(cmd, rest)?;
            Step::TypeEnv { widget, var }
        }
        "save-text" => {
            let (text, path) = two_args(cmd, rest)?;
            Step::SaveText { text, path }
        }
        other => return Err(format!("unknown step '{other}'")),
    })
}

/// `<seconds> <text>`.
fn seconds_and_text(cmd: &str, rest: &str) -> Result<(u64, String), String> {
    let (secs, text) = rest
        .split_once(char::is_whitespace)
        .ok_or_else(|| format!("'{cmd}' needs seconds and text"))?;
    let secs = secs
        .parse()
        .map_err(|_| format!("'{cmd}' needs seconds, got '{secs}'"))?;
    Ok((secs, text.trim().to_string()))
}

/// `<first> <rest>`, where `<first>` may be in double quotes.
fn two_args(cmd: &str, rest: &str) -> Result<(String, String), String> {
    let (first, second) = if let Some(quoted) = rest.strip_prefix('"') {
        quoted
            .split_once('"')
            .ok_or("unterminated quote in widget title")?
    } else {
        rest.split_once(char::is_whitespace)
            .ok_or_else(|| format!("'{cmd}' needs two arguments"))?
    };
    let second = second.trim();
    if first.is_empty() || second.is_empty() {
        return Err(format!("'{cmd}' needs two arguments"));
    }
    Ok((first.to_string(), second.to_string()))
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Step::Click(l) => write!(f, "click {l}"),
            Step::ClickContaining(t) => write!(f, "click-containing {t}"),
            Step::Type { widget, text } => write!(f, "type \"{widget}\" {text}"),
            // The name only: the value may be long, and is the caller's.
            Step::TypeEnv { widget, var } => write!(f, "type-env \"{widget}\" {var}"),
            Step::SaveText { text, path } => write!(f, "save-text \"{text}\" {path}"),
            Step::Expect(t) => write!(f, "expect {t}"),
            Step::ExpectSlow(t) => write!(f, "expect-slow {t}"),
            Step::ExpectWithin { secs, text } => write!(f, "expect-within {secs} {text}"),
            Step::ExpectGoneWithin { secs, text } => {
                write!(f, "expect-gone-within {secs} {text}")
            }
            Step::ExpectClipboard(t) => write!(f, "expect-clipboard {t}"),
            Step::ExpectClipboardEmpty => write!(f, "expect-clipboard-empty"),
            Step::ExpectNumbered(n) => write!(f, "expect-numbered {n}"),
            Step::ExpectNot(t) => write!(f, "expect-not {t}"),
            Step::ExpectAbsent(t) => write!(f, "expect-absent {t}"),
            Step::ExpectLocked => write!(f, "expect-locked"),
            Step::ExpectPinned => write!(f, "expect-pinned"),
            Step::ExpectRowControls { text, enabled } => {
                let cmd = if *enabled { "enabled" } else { "disabled" };
                write!(f, "expect-{cmd} {text}")
            }
            Step::Apply(t) => write!(f, "apply {t}"),
            Step::CloseDialog => write!(f, "close-dialog"),
            Step::ScrollEnd => write!(f, "scroll-end"),
            Step::Activate => write!(f, "activate"),
            Step::ExpectWindows(n) => write!(f, "expect-windows {n}"),
            Step::Wait(ms) => write!(f, "wait {ms}"),
            Step::Shot(n) => write!(f, "shot {n}"),
            Step::Action(a) => write!(f, "action {a}"),
        }
    }
}

static FAILED: AtomicBool = AtomicBool::new(false);

/// Whether a step failed, which makes the process exit 1.
pub fn failed() -> bool {
    FAILED.load(Ordering::SeqCst)
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
                FAILED.store(true, Ordering::SeqCst);
                // The road a user's Quit takes, so a real wallet closes cleanly.
                env.app.activate_action("quit", None);
                return;
            }
        }
        println!("DRIVE OK");
        let _ = std::io::stdout().flush();
        env.app.activate_action("quit", None);
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
        Step::ClickContaining(text) => {
            poll(|| click_containing(env, text).then_some(()))
                .await
                .ok_or_else(|| format!("no sensitive visible button containing '{text}'"))?;
            glib::timeout_future(Duration::from_millis(150)).await;
        }
        Step::Type { widget, text } => {
            poll(|| type_into(env, widget, text).then_some(()))
                .await
                .ok_or_else(|| format!("no entry titled '{widget}'"))?;
            glib::timeout_future(Duration::from_millis(150)).await;
        }
        Step::TypeEnv { widget, var } => {
            let text = std::env::var(var).map_err(|_| format!("{var} is not set"))?;
            poll(|| type_into(env, widget, &text).then_some(()))
                .await
                .ok_or_else(|| format!("no entry titled '{widget}'"))?;
            glib::timeout_future(Duration::from_millis(150)).await;
        }
        Step::SaveText { text, path } => {
            let found = poll(|| label_containing(env, text))
                .await
                .ok_or_else(|| format!("no visible label containing '{text}'"))?;
            std::fs::write(path, found).map_err(|e| format!("{path}: {e}"))?;
        }
        Step::ExpectSlow(text) => {
            poll_up_to(SLOW_POLL_LIMIT, || {
                has_label_containing(env, text).then_some(())
            })
            .await
            .ok_or_else(|| format!("no visible label containing '{text}'"))?;
        }
        Step::ExpectWithin { secs, text } => {
            poll_up_to(polls_in(*secs), || {
                has_label_containing(env, text).then_some(())
            })
            .await
            .ok_or_else(|| format!("no visible label containing '{text}' within {secs} s"))?;
        }
        Step::ExpectGoneWithin { secs, text } => {
            poll_up_to(polls_in(*secs), || {
                (!has_label_containing(env, text)).then_some(())
            })
            .await
            .ok_or_else(|| format!("a visible label still contains '{text}' after {secs} s"))?;
        }
        Step::ExpectNumbered(n) => {
            poll(|| numbered_words_are(&mapped_labels(env), *n).then_some(()))
                .await
                .ok_or_else(|| format!("not exactly {n} numbered words"))?;
        }
        Step::ExpectClipboard(text) => {
            let held = clipboard_text(env).await;
            if !held.as_deref().is_some_and(|t| t.contains(text.as_str())) {
                return Err(format!("the clipboard does not hold '{text}'"));
            }
        }
        Step::ExpectClipboardEmpty => {
            if clipboard_text(env).await.is_some() {
                return Err("the clipboard still holds text".to_string());
            }
        }
        Step::Expect(text) => {
            poll(|| has_label_containing(env, text).then_some(()))
                .await
                .ok_or_else(|| format!("no visible label containing '{text}'"))?;
        }
        Step::ExpectNot(text) => {
            poll(|| (!has_label_containing(env, text)).then_some(()))
                .await
                .ok_or_else(|| format!("a visible label still contains '{text}'"))?;
        }
        Step::ExpectAbsent(text) => {
            let held = roots(env).iter().any(|r| {
                descendants(r).iter().any(|d| {
                    d.downcast_ref::<gtk::Label>()
                        .is_some_and(|l| l.text().contains(text.as_str()))
                })
            });
            if held {
                return Err(format!("a label holds '{text}'"));
            }
        }
        Step::ExpectLocked => {
            poll(|| locked(env).then_some(()))
                .await
                .ok_or("the dialog can still be closed or popped")?;
        }
        Step::ExpectPinned => {
            poll(|| pinned(env).then_some(()))
                .await
                .ok_or("the page can still be popped")?;
        }
        Step::ExpectRowControls { text, enabled } => {
            poll(|| (row_controls(env, text) == Some(*enabled)).then_some(()))
                .await
                .ok_or_else(|| {
                    let want = if *enabled { "usable" } else { "unusable" };
                    format!("no row with '{text}' whose controls are all {want}")
                })?;
        }
        Step::Apply(title) => {
            poll(|| apply_entry(env, title).then_some(()))
                .await
                .ok_or_else(|| format!("no entry titled '{title}'"))?;
            glib::timeout_future(Duration::from_millis(150)).await;
        }
        Step::CloseDialog => {
            let dialog = env.window.visible_dialog().ok_or("no dialog is open")?;
            if !dialog.close() {
                return Err("the dialog refused to close".to_string());
            }
            glib::timeout_future(Duration::from_millis(150)).await;
        }
        Step::ScrollEnd => {
            // Layout may still be growing the content.
            glib::timeout_future(Duration::from_millis(200)).await;
            for root in roots(env) {
                for w in descendants(&root).iter().filter(|w| live(w)) {
                    if let Some(sw) = w.downcast_ref::<gtk::ScrolledWindow>() {
                        let adj = sw.vadjustment();
                        adj.set_value(adj.upper() - adj.page_size());
                    }
                }
            }
            glib::timeout_future(Duration::from_millis(150)).await;
        }
        Step::Activate => {
            env.app.activate();
            glib::timeout_future(Duration::from_millis(150)).await;
        }
        Step::ExpectWindows(n) => {
            let windows = env.app.windows().len();
            if windows != *n {
                return Err(format!("{windows} windows"));
            }
        }
        Step::Wait(ms) => glib::timeout_future(Duration::from_millis(*ms)).await,
        Step::Shot(name) => shot(name).await,
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

/// How many polls make `secs` seconds.
fn polls_in(secs: u64) -> u32 {
    let polls = secs.saturating_mul(1000) / POLL.as_millis() as u64;
    u32::try_from(polls).unwrap_or(u32::MAX)
}

async fn poll<T>(f: impl FnMut() -> Option<T>) -> Option<T> {
    poll_up_to(POLL_LIMIT, f).await
}

async fn poll_up_to<T>(limit: u32, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    for _ in 0..limit {
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
    let got_line = first_of(line, timeout).await;
    if !got_line {
        glib::timeout_future(SHOT_PAUSE).await;
    }
}

/// Resolve with whichever future finishes first.
async fn first_of(
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

fn locked(env: &Env) -> bool {
    let Some(dialog) = env.window.visible_dialog() else {
        return false;
    };
    let pinned = descendants(dialog.upcast_ref())
        .into_iter()
        .filter_map(|w| w.downcast::<adw::NavigationView>().ok())
        .filter_map(|nav| nav.visible_page())
        .all(|page| !page.can_pop());
    pinned && !dialog.can_close()
}

fn pinned(env: &Env) -> bool {
    descendants(env.window.upcast_ref())
        .into_iter()
        .filter_map(|w| w.downcast::<adw::NavigationView>().ok())
        .filter(|nav| nav.is_mapped() && nav.navigation_stack().n_items() > 1)
        .filter_map(|nav| nav.visible_page())
        .any(|page| !page.can_pop())
}

/// Whether the controls of the mapped list row showing `text` can be used:
/// `Some(true)` if all can, `Some(false)` if none can, `None` if the row is
/// missing, has no controls, or is mixed.
fn row_controls(env: &Env, text: &str) -> Option<bool> {
    for root in roots(env) {
        for w in descendants(&root) {
            if !w.is_mapped() || w.downcast_ref::<gtk::ListBoxRow>().is_none() {
                continue;
            }
            if !label_texts(&w).iter().any(|t| t.contains(text)) {
                continue;
            }
            let controls: Vec<bool> = descendants(&w)
                .iter()
                .filter(|d| d.is::<gtk::MenuButton>() || d.is::<gtk::Button>())
                .map(|d| d.is_sensitive())
                .collect();
            if controls.is_empty() {
                continue;
            }
            if controls.iter().all(|c| *c) {
                return Some(true);
            }
            if controls.iter().all(|c| !*c) {
                return Some(false);
            }
            return None;
        }
    }
    None
}

fn apply_entry(env: &Env, title: &str) -> bool {
    for root in roots(env) {
        for w in descendants(&root).iter().filter(|w| live(w)) {
            if let Some(row) = w.downcast_ref::<adw::EntryRow>()
                && row.title() == title
            {
                row.emit_by_name::<()>("apply", &[]);
                return true;
            }
        }
    }
    false
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

/// Labels and entry contents count as visible text.
fn has_label_containing(env: &Env, text: &str) -> bool {
    roots(env).iter().any(|r| {
        descendants(r).iter().any(|d| {
            let label = d
                .downcast_ref::<gtk::Label>()
                .is_some_and(|l| l.text().contains(text));
            let entry = d
                .downcast_ref::<gtk::Text>()
                .is_some_and(|t| t.text().contains(text));
            d.is_mapped() && (label || entry)
        })
    })
}

/// The whole text of the first mapped label containing `text`.
fn label_containing(env: &Env, text: &str) -> Option<String> {
    roots(env).iter().find_map(|r| {
        descendants(r).iter().find_map(|d| {
            let label = d.downcast_ref::<gtk::Label>()?;
            (d.is_mapped() && label.text().contains(text)).then(|| label.text().to_string())
        })
    })
}

/// Whether the labels hold exactly the words `1. x` to `<n>. x`.
fn numbered_words_are(labels: &[String], n: u32) -> bool {
    let mut found: Vec<u32> = labels
        .iter()
        .filter_map(|label| {
            let (number, word) = label.split_once(". ")?;
            let is_word = !word.is_empty() && word.chars().all(char::is_alphabetic);
            is_word.then(|| number.parse().ok()).flatten()
        })
        .collect();
    found.sort_unstable();
    found.into_iter().eq(1..=n)
}

fn mapped_labels(env: &Env) -> Vec<String> {
    roots(env).iter().flat_map(label_texts).collect()
}

/// What a paste would give, as this process sees it; `None` for no text.
async fn clipboard_text(env: &Env) -> Option<String> {
    let text = env.window.clipboard().read_text_future().await;
    text.ok().flatten().map(|t| t.to_string())
}

fn title_matches(w: &gtk::Widget, label: &str) -> bool {
    w.downcast_ref::<adw::PreferencesRow>()
        .is_some_and(|r| r.title() == label)
        || label_texts(w).iter().any(|t| t == label)
}

/// Click the first live button whose own label contains `text`.
fn click_containing(env: &Env, text: &str) -> bool {
    for root in roots(env) {
        for w in descendants(&root).iter().filter(|w| live(w)) {
            if let Some(b) = w.downcast_ref::<gtk::Button>()
                && b.label().is_some_and(|l| l.contains(text))
            {
                b.emit_clicked();
                return true;
            }
        }
    }
    false
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
                .filter(|n| n.visible_page().is_some_and(|p| p.can_pop()))
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
click-containing moving to Saving

type \"Amount (BTC)\" 0.01
type Target 0.1 extra words
type-env \"Paste an invite code\" FM_INVITE_CODE
save-text lnbcrt invoice.txt
save-text \"two words\" /tmp/out.txt
expect Fedi Alpha
expect-slow Fedi Beta
expect-within 300 Txid
expect-gone-within 120 Offline
expect-clipboard demo
expect-clipboard-empty
expect-not Cover traffic
expect-absent bc1q
expect-locked
expect-pinned
expect-disabled waiting after reconnect
expect-enabled waiting for lower fees
apply Server URL
close-dialog
scroll-end
activate
expect-windows 1
wait 250
shot funded-moving
action app.settings
";
        assert_eq!(
            parse(src).unwrap(),
            vec![
                Step::Click("Exit now (lower privacy)".into()),
                Step::ClickContaining("moving to Saving".into()),
                Step::Type {
                    widget: "Amount (BTC)".into(),
                    text: "0.01".into()
                },
                Step::Type {
                    widget: "Target".into(),
                    text: "0.1 extra words".into()
                },
                Step::TypeEnv {
                    widget: "Paste an invite code".into(),
                    var: "FM_INVITE_CODE".into()
                },
                Step::SaveText {
                    text: "lnbcrt".into(),
                    path: "invoice.txt".into()
                },
                Step::SaveText {
                    text: "two words".into(),
                    path: "/tmp/out.txt".into()
                },
                Step::Expect("Fedi Alpha".into()),
                Step::ExpectSlow("Fedi Beta".into()),
                Step::ExpectWithin {
                    secs: 300,
                    text: "Txid".into()
                },
                Step::ExpectGoneWithin {
                    secs: 120,
                    text: "Offline".into()
                },
                Step::ExpectClipboard("demo".into()),
                Step::ExpectClipboardEmpty,
                Step::ExpectNot("Cover traffic".into()),
                Step::ExpectAbsent("bc1q".into()),
                Step::ExpectLocked,
                Step::ExpectPinned,
                Step::ExpectRowControls {
                    text: "waiting after reconnect".into(),
                    enabled: false
                },
                Step::ExpectRowControls {
                    text: "waiting for lower fees".into(),
                    enabled: true
                },
                Step::Apply("Server URL".into()),
                Step::CloseDialog,
                Step::ScrollEnd,
                Step::Activate,
                Step::ExpectWindows(1),
                Step::Wait(250),
                Step::Shot("funded-moving".into()),
                Step::Action("app.settings".into()),
            ]
        );
    }

    #[test]
    fn numbered_words_are_counted_exactly() {
        let chips = |n: u32| -> Vec<String> { (1..=n).map(|k| format!("{k}. word")).collect() };
        assert!(numbered_words_are(&chips(12), 12));
        assert!(!numbered_words_are(&chips(11), 12));
        assert!(!numbered_words_are(&chips(13), 12));
        // Text that merely has the number in it is not a word.
        let mut other = chips(11);
        other.push("0.0012. BTC".to_string());
        assert!(!numbered_words_are(&other, 12));
        // Other labels on the page do not count.
        let mut page = chips(12);
        page.push("Write these words down.".to_string());
        assert!(numbered_words_are(&page, 12));
    }

    #[test]
    fn rejects_bad_steps_with_line_number() {
        for bad in [
            "bogus x",
            "click",
            "wait soon",
            "type onlywidget",
            "type-env onlywidget",
            "save-text onlytext",
            "expect-slow",
            "expect-within Txid",
            "expect-within 300",
            "expect-gone-within soon Offline",
            "expect-clipboard",
            "expect-numbered twelve",
            "shot",
            "type \"open",
        ] {
            let err = parse(bad).unwrap_err();
            assert!(err.contains("line 1"), "{bad}: {err}");
        }
        assert!(parse("click ok\nwait x").unwrap_err().contains("line 2"));
    }
}
