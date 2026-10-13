//! iamlate: find the meeting happening right now in macOS Calendar and join it.
//!
//! Reads events via EventKit from the accounts synced to Calendar.app (Google, Exchange, ...),
//! extracts the video link, and joins directly:
//!   - Zoom   -> zoommtg:// (native app, joins without the browser hop)
//!   - Teams  -> msteams:   (native app)
//!   - Meet   -> Chrome, then auto-clicks "Join now" (needs Chrome's View > Developer >
//!     Allow JavaScript from Apple Events)
//!   - Webex / others -> opened as-is

use std::io::IsTerminal;
use std::process::{Command, ExitCode};
use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use chrono::{DateTime, Local, TimeZone};
use console::style;
use dialoguer::{Select, theme::ColorfulTheme};
use objc2::rc::Retained;
use objc2::runtime::Bool;
use objc2_event_kit::{EKCalendar, EKEntityType, EKEvent, EKEventStore, EKParticipantStatus};
use objc2_foundation::{NSArray, NSDate, NSError};
use regex::Regex;

/// How early before its start a meeting counts as "current".
const EARLY_SECS: f64 = 10.0 * 60.0;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
enum Kind {
    Zoom,
    Teams,
    Meet,
    Webex,
    #[default]
    Other,
}

/// ~/.iamlate.config, e.g.:
///   account = me@example.com          # only use this calendar account (default: all)
///   room.team = https://zoom.us/j/1   # `iamlate team [name]`; the first room is the default
#[derive(Default)]
struct Config {
    path: String,
    account: Option<String>,
    rooms: Vec<(String, String)>,
}

impl Config {
    fn load() -> Self {
        let path = std::env::var("HOME")
            .map(|h| format!("{h}/.iamlate.config"))
            .unwrap_or_default();
        let mut config = Config::default();
        for line in std::fs::read_to_string(&path).unwrap_or_default().lines() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let (key, value) = (key.trim(), value.trim().to_string());
            if key == "account" {
                config.account = Some(value);
            } else if let Some(name) = key.strip_prefix("room.") {
                config.rooms.push((name.to_string(), value));
            }
        }
        config.path = path;
        config
    }
}

#[derive(Default)]
struct Meeting {
    title: String,
    starts_in: f64,
    url: String,
    kind: Kind,
    start: DateTime<Local>,
    end: DateTime<Local>,
    recurring: bool,
    calendar: String,
    organizer: Option<String>,
    attendees: usize,
    response: Option<&'static str>,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dry_run = args.iter().any(|a| a == "-n" || a == "--dry-run");
    let debug = args.iter().any(|a| a == "--debug");
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!(
            "usage: iamlate [team [ROOM]] [-n|--dry-run] [--debug]\n\n\
             Joins the current (or about-to-start) meeting from macOS Calendar.\n\
             `iamlate team [ROOM]` joins a room from ~/.iamlate.config (`room.NAME = URL`, first is default).\n\
             Set `account = you@example.com` there to only use that calendar account."
        );
        return ExitCode::SUCCESS;
    }
    let config = Config::load();
    if let Some(i) = args.iter().position(|a| a == "team") {
        let name = args.get(i + 1).filter(|a| !a.starts_with('-'));
        return join_team_room(&config, name.map(String::as_str), dry_run);
    }

    let store = unsafe { EKEventStore::new() };
    if !request_access(&store) {
        eprintln!(
            "No calendar access. Grant it in System Settings > Privacy & Security > Calendars."
        );
        return ExitCode::FAILURE;
    }

    let calendars = match &config.account {
        Some(account) => {
            let calendars = account_calendars(&store, account);
            if calendars.is_empty() {
                eprintln!(
                    "No calendars for {account} in macOS Calendar. Add the account in System Settings > Internet Accounts."
                );
                return ExitCode::FAILURE;
            }
            Some(calendars)
        }
        None => None,
    };

    let mut meetings = current_meetings(&store, calendars.as_deref(), debug);
    if meetings.is_empty() {
        eprintln!("No meeting with a video link right now. You're not late. 🎉");
        return ExitCode::FAILURE;
    }
    rank(&mut meetings);

    if dry_run {
        for m in &meetings {
            println!("→ {}\n  {}", label(m, true), m.url);
        }
        return ExitCode::SUCCESS;
    }
    let m = if meetings.len() == 1 {
        &meetings[0]
    } else {
        match choose(&meetings) {
            Some(i) => &meetings[i],
            None => return ExitCode::FAILURE,
        }
    };
    println!("→ {}\n  {}", label(m, true), m.url);
    join(m);
    ExitCode::SUCCESS
}

/// One-time meetings before recurring ones, then closest start first (just started or about
/// to start); the same link in several calendars counts once.
fn rank(meetings: &mut Vec<Meeting>) {
    meetings.sort_by(|a, b| {
        a.recurring
            .cmp(&b.recurring)
            .then(a.starts_in.abs().total_cmp(&b.starts_in.abs()))
    });
    let mut seen = std::collections::HashSet::new();
    meetings.retain(|m| seen.insert(m.url.clone()));
}

fn when(m: &Meeting) -> String {
    if m.starts_in > 0.0 {
        format!("in {} min", (m.starts_in / 60.0).ceil() as i64)
    } else {
        format!("{} min ago", (-m.starts_in / 60.0).floor() as i64)
    }
}

/// One line per meeting: time, status, provider, title, and details.
fn label(m: &Meeting, styled: bool) -> String {
    let mut details = vec![if m.recurring { "recurring" } else { "one-time" }.to_string()];
    if m.attendees > 0 {
        details.push(format!("{} people", m.attendees));
    }
    if let Some(organizer) = &m.organizer {
        details.push(format!("by {organizer}"));
    }
    if let Some(response) = m.response {
        details.push(response.to_string());
    }
    if !m.calendar.is_empty() {
        details.push(m.calendar.clone());
    }
    let time = format!("{}–{}", m.start.format("%H:%M"), m.end.format("%H:%M"));
    let when = format!("{:<11}", when(m));
    let kind = format!("{:<5}", format!("{:?}", m.kind));
    let details = details.join(" · ");
    if !styled {
        return format!("{time}  {when}  {kind}  {}  ({details})", m.title);
    }
    let when = if m.starts_in > 0.0 {
        style(when).green()
    } else {
        style(when).yellow()
    };
    format!(
        "{}  {}  {}  {}  {}",
        style(time).dim(),
        when,
        style(kind).cyan(),
        style(&m.title).bold(),
        style(details).dim()
    )
}

/// Several meetings at once: arrow-key selector in the terminal, or a dialog when run from a hotkey.
fn choose(meetings: &[Meeting]) -> Option<usize> {
    if std::io::stdin().is_terminal() {
        let labels: Vec<String> = meetings.iter().map(|m| label(m, true)).collect();
        return Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Several meetings right now. Which one?")
            .items(&labels)
            .default(0)
            .interact_opt()
            .ok()
            .flatten();
    }

    let labels: Vec<String> = meetings.iter().map(|m| label(m, false)).collect();
    let quote = |l: &String| format!("\"{}\"", l.replace('\\', "\\\\").replace('"', "\\\""));
    let items: Vec<String> = labels.iter().map(quote).collect();
    let script = format!(
        "choose from list {{{}}} with title \"iamlate\" with prompt \"Several meetings right now. Which one?\" default items {{{}}}",
        items.join(", "),
        items[0]
    );
    let out = Command::new("osascript")
        .args(["-e", &script])
        .output()
        .ok()?;
    let picked = String::from_utf8_lossy(&out.stdout).trim().to_string();
    labels.iter().position(|l| *l == picked)
}

fn join_team_room(config: &Config, name: Option<&str>, dry_run: bool) -> ExitCode {
    let rooms = &config.rooms;
    let room = match name {
        Some(name) => rooms.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)),
        None => rooms.first(),
    };
    let Some((title, url)) = room else {
        let known: Vec<&str> = rooms.iter().map(|(n, _)| n.as_str()).collect();
        eprintln!(
            "Room not found. Add `room.NAME = URL` to {}. Known: {known:?}",
            config.path
        );
        return ExitCode::FAILURE;
    };
    let (url, kind) = find_link_in(url).unwrap_or((url.clone(), Kind::Other));
    let m = Meeting {
        title: title.clone(),
        url,
        kind,
        ..Default::default()
    };
    println!("→ {} [{:?}]\n  {}", m.title, m.kind, m.url);
    if !dry_run {
        join(&m);
    }
    ExitCode::SUCCESS
}

fn request_access(store: &EKEventStore) -> bool {
    let (tx, rx) = mpsc::channel();
    let block = RcBlock::new(move |granted: Bool, _err: *mut NSError| {
        let _ = tx.send(granted.as_bool());
    });
    unsafe { store.requestFullAccessToEventsWithCompletion(RcBlock::as_ptr(&block)) };
    rx.recv_timeout(Duration::from_secs(60)).unwrap_or(false)
}

/// Calendars belonging to `account`: matched by account (source) name or calendar name.
fn account_calendars(store: &EKEventStore, account: &str) -> Vec<Retained<EKCalendar>> {
    let calendars = unsafe { store.calendarsForEntityType(EKEntityType::Event) };
    calendars
        .iter()
        .filter(|c| unsafe {
            c.title().to_string().eq_ignore_ascii_case(account)
                || c.source()
                    .is_some_and(|s| s.title().to_string().eq_ignore_ascii_case(account))
        })
        .collect()
}

fn current_meetings(
    store: &EKEventStore,
    calendars: Option<&[Retained<EKCalendar>]>,
    debug: bool,
) -> Vec<Meeting> {
    let events = unsafe {
        let start = NSDate::dateWithTimeIntervalSinceNow(-12.0 * 3600.0);
        let end = NSDate::dateWithTimeIntervalSinceNow(EARLY_SECS);
        let calendars = calendars.map(NSArray::from_retained_slice);
        let predicate = store.predicateForEventsWithStartDate_endDate_calendars(
            &start,
            &end,
            calendars.as_deref(),
        );
        store.eventsMatchingPredicate(&predicate)
    };
    if debug {
        let status = unsafe { EKEventStore::authorizationStatusForEntityType(EKEntityType::Event) };
        eprintln!(
            "auth status: {status:?}, events in window: {}",
            events.count()
        );
        for e in events.iter() {
            let (title, starts_in) =
                unsafe { (e.title().to_string(), e.startDate().timeIntervalSinceNow()) };
            eprintln!(
                "  {:>6.0} min  {title}  link={:?}",
                starts_in / 60.0,
                find_link(&e).map(|l| l.1)
            );
        }
    }

    events
        .iter()
        .filter_map(|event| {
            let (all_day, starts_in, ends_in) = unsafe {
                (
                    event.isAllDay(),
                    event.startDate().timeIntervalSinceNow(),
                    event.endDate().timeIntervalSinceNow(),
                )
            };
            if all_day || ends_in <= 0.0 || starts_in > EARLY_SECS || declined(&event) {
                return None;
            }
            let (url, kind) = find_link(&event)?;
            Some(meeting(&event, starts_in, url, kind))
        })
        .collect()
}

fn meeting(event: &EKEvent, starts_in: f64, url: String, kind: Kind) -> Meeting {
    unsafe {
        let local = |date: Retained<NSDate>| {
            Local
                .timestamp_opt(date.timeIntervalSince1970() as i64, 0)
                .single()
                .unwrap_or_default()
        };
        let me = event
            .attendees()
            .and_then(|a| a.iter().find(|p| p.isCurrentUser()));
        let response = me.and_then(|p| match p.participantStatus() {
            EKParticipantStatus::Accepted => Some("accepted"),
            EKParticipantStatus::Tentative => Some("tentative"),
            EKParticipantStatus::Pending => Some("not answered"),
            _ => None,
        });
        Meeting {
            title: event.title().to_string(),
            starts_in,
            url,
            kind,
            start: local(event.startDate()),
            end: local(event.endDate()),
            recurring: event.hasRecurrenceRules(),
            calendar: event
                .calendar()
                .map(|c| c.title().to_string())
                .unwrap_or_default(),
            organizer: event
                .organizer()
                .filter(|o| !o.isCurrentUser())
                .and_then(|o| o.name())
                .map(|n| n.to_string()),
            attendees: event.attendees().map(|a| a.count()).unwrap_or(0),
            response,
        }
    }
}

fn declined(event: &EKEvent) -> bool {
    let Some(attendees) = (unsafe { event.attendees() }) else {
        return false;
    };
    attendees.iter().any(|p| unsafe {
        p.isCurrentUser() && p.participantStatus() == EKParticipantStatus::Declined
    })
}

/// Look through URL, location and notes for a known video-conference link.
fn find_link(event: &Retained<EKEvent>) -> Option<(String, Kind)> {
    let mut haystack = String::new();
    unsafe {
        if let Some(url) = event.URL().and_then(|u| u.absoluteString()) {
            haystack.push_str(&url.to_string());
            haystack.push('\n');
        }
        if let Some(loc) = event.location() {
            haystack.push_str(&loc.to_string());
            haystack.push('\n');
        }
        if let Some(notes) = event.notes() {
            haystack.push_str(&notes.to_string());
        }
    }
    find_link_in(&haystack)
}

fn find_link_in(haystack: &str) -> Option<(String, Kind)> {
    let patterns: [(Kind, &str); 4] = [
        (Kind::Zoom, r"https://[\w.-]*zoom\.us/(?:j|my|w)/[^\s<>]+"),
        (
            Kind::Teams,
            r"https://teams\.(?:microsoft|live)\.com/(?:l/meetup-join|meet)/[^\s<>]+",
        ),
        (
            Kind::Meet,
            r"https://meet\.google\.com/[a-z]{3}-[a-z]{4}-[a-z]{3}",
        ),
        (Kind::Webex, r"https://[\w.-]+\.webex\.com/[^\s<>]+"),
    ];
    for (kind, pattern) in patterns {
        if let Some(m) = Regex::new(pattern).unwrap().find(haystack) {
            return Some((clean(m.as_str()), kind));
        }
    }
    // Fallback: an explicit event URL that isn't a calendar link.
    let fallback = Regex::new(r"https://(?:whereby\.com|meet\.jit\.si|[\w.-]*gotomeeting\.com|app\.slack\.com/huddle)/[^\s<>]+").unwrap();
    fallback
        .find(haystack)
        .map(|m| (clean(m.as_str()), Kind::Other))
}

/// Strip trailing punctuation that often sticks to links in notes.
fn clean(url: &str) -> String {
    url.trim_end_matches([')', '>', '"', '\'', '.', ',', ';', ']'])
        .to_string()
}

fn join(m: &Meeting) {
    match m.kind {
        Kind::Zoom => open(&zoom_native(&m.url).unwrap_or_else(|| m.url.clone())),
        Kind::Teams if app_installed("Microsoft Teams") => {
            open(&m.url.replacen("https://", "msteams://", 1))
        }
        Kind::Meet => join_meet(&m.url),
        _ => open(&m.url),
    }
}

/// https://acme.zoom.us/j/123?pwd=abc -> zoommtg://acme.zoom.us/join?action=join&confno=123&pwd=abc
fn zoom_native(url: &str) -> Option<String> {
    let re = Regex::new(r"https://([\w.-]*zoom\.us)/j/(\d+)(?:\?(?:.*&)?pwd=([\w.-]+))?").unwrap();
    let c = re.captures(url)?;
    let mut native = format!("zoommtg://{}/join?action=join&confno={}", &c[1], &c[2]);
    if let Some(pwd) = c.get(3) {
        native.push_str("&pwd=");
        native.push_str(pwd.as_str());
    }
    Some(native)
}

fn join_meet(url: &str) {
    if !app_installed("Google Chrome") {
        open(url);
        return;
    }
    let _ = Command::new("open")
        .args(["-a", "Google Chrome", url])
        .status();

    // Click "Join now" / "Ask to join" once the pre-join screen has loaded.
    let script = r#"
tell application "Google Chrome"
  repeat 40 times
    delay 0.5
    try
      set r to execute active tab of front window javascript "(() => { const labels = /^(Join now|Ask to join|Join here too|Jetzt teilnehmen|Teilnahme anfragen|Hier auch teilnehmen)$/; const b = [...document.querySelectorAll('button')].find(b => labels.test(b.innerText.trim())); if (b) { b.click(); return 'ok'; } return ''; })()"
      if r is "ok" then return "ok"
    end try
  end repeat
  return "timeout"
end tell"#;
    match Command::new("osascript").args(["-e", script]).output() {
        Ok(out) if String::from_utf8_lossy(&out.stdout).trim() == "ok" => println!("  joined ✔"),
        _ => eprintln!(
            "  Could not auto-click Join. Enable Chrome > View > Developer > Allow JavaScript from Apple Events."
        ),
    }
}

fn app_installed(name: &str) -> bool {
    ["/Applications", "/System/Applications"]
        .iter()
        .any(|dir| std::path::Path::new(&format!("{dir}/{name}.app")).exists())
        || std::env::var("HOME")
            .map(|h| std::path::Path::new(&format!("{h}/Applications/{name}.app")).exists())
            .unwrap_or(false)
}

fn open(target: &str) {
    if let Err(e) = Command::new("open").arg(target).status() {
        eprintln!("failed to open {target}: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_links_become_native() {
        assert_eq!(
            zoom_native("https://acme.zoom.us/j/123456789?pwd=AbC.1").as_deref(),
            Some("zoommtg://acme.zoom.us/join?action=join&confno=123456789&pwd=AbC.1")
        );
        assert_eq!(
            zoom_native("https://zoom.us/j/42").as_deref(),
            Some("zoommtg://zoom.us/join?action=join&confno=42")
        );
    }

    fn meeting(title: &str, starts_in_min: f64, recurring: bool, url: &str) -> Meeting {
        Meeting {
            title: title.into(),
            starts_in: starts_in_min * 60.0,
            recurring,
            url: url.into(),
            ..Default::default()
        }
    }

    #[test]
    fn one_time_meetings_come_before_recurring_ones() {
        let mut meetings = vec![
            meeting("Daily", -1.0, true, "a"),
            meeting("Customer call", 8.0, false, "b"),
            meeting("Interview", -20.0, false, "c"),
            meeting("Daily (other calendar)", -1.0, true, "a"),
        ];
        rank(&mut meetings);
        let titles: Vec<&str> = meetings.iter().map(|m| m.title.as_str()).collect();
        assert_eq!(titles, ["Customer call", "Interview", "Daily"]);
    }

    #[test]
    fn label_shows_details() {
        let mut m = meeting("Customer call", 3.0, false, "x");
        m.kind = Kind::Zoom;
        m.attendees = 4;
        m.organizer = Some("Jane Doe".into());
        m.response = Some("accepted");
        m.calendar = "Work".into();
        let label = label(&m, false);
        assert!(label.contains("in 3 min"), "{label}");
        assert!(
            label.ends_with(
                "Zoom   Customer call  (one-time · 4 people · by Jane Doe · accepted · Work)"
            ),
            "{label}"
        );
    }

    #[test]
    fn trailing_punctuation_is_stripped() {
        assert_eq!(
            clean("https://meet.google.com/abc-defg-hij)."),
            "https://meet.google.com/abc-defg-hij"
        );
    }
}
