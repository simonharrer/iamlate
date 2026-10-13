# iamlate

Late for a meeting? Run `iamlate` and you're in.

`iamlate` looks up the meeting that is happening right now (or starts within the next 10 minutes) in your macOS calendar, finds the video link, and joins it directly:

| Provider    | How it joins                                                                 |
|-------------|------------------------------------------------------------------------------|
| Zoom        | Native Zoom app via `zoommtg://`, no browser detour                          |
| Teams       | Native Teams app via `msteams://` (browser if Teams isn't installed)         |
| Google Meet | Opens Chrome and clicks **Join now** for you                                 |
| Webex, Whereby, Jitsi, GoTo | Opens the link                                               |

All-day events and events you declined are skipped.

## Install

Requires macOS 14+ and Rust.

```sh
cargo install --git https://github.com/simonharrer/iamlate
```

On first run macOS asks for calendar access. `iamlate` reads events through EventKit, so it sees every account you added to Calendar.app (System Settings › Internet Accounts: Google, Microsoft Exchange/Outlook, iCloud, ...).

For Google Meet auto-join, enable **View › Developer › Allow JavaScript from Apple Events** in Chrome once. Otherwise you end up on the Meet pre-join screen.

## Usage

```sh
iamlate              # join the current meeting
iamlate team         # join your default team room
iamlate team backup  # join a named room
iamlate -n           # dry run: show what would be joined
iamlate --debug      # list the events iamlate sees
```

Bind `iamlate` to a hotkey (Raycast, Alfred, Shortcuts) for one-keystroke lateness.

## Config

Optional, in `~/.iamlate.config`:

```ini
# Only use events from this calendar account (default: all calendars)
account = you@example.com

# Rooms for `iamlate team [name]`; the first one is the default
room.team = https://us05web.zoom.us/j/123456789?pwd=secret
room.backup = https://meet.google.com/abc-defg-hij
```

`account` matches either the account name or the calendar name shown in Calendar.app.
