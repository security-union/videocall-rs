# Searching for Meetings

The search modal lets you find any meeting you have access to — active, ended, or waiting to start — without leaving the current page.

## Opening search

Three ways to open the search modal:

| Method | Platform |
|--------|----------|
| **Cmd-K** keyboard shortcut | macOS |
| **Ctrl-K** keyboard shortcut | Windows, Linux |
| Click the **🔍 Search** button in the top bar | Any |

Press **Esc** or click outside the modal to close it.

## What you can search

The search box matches against any of these meeting attributes as you type:

- **Meeting ID** (the room name, e.g. `standup-2026`)
- **Host display name** (e.g. `Alice Chen`)
- **Meeting description** (auto-generated, includes the meeting ID and state)

Search is **case-insensitive** and **typeahead**: results update after each keystroke. You do not need to press Enter.

### Partial matches

You can type just the beginning of a word and meetings containing that prefix will match. For example:

- `stand` → matches `standup-2026`, `standing-meeting`, `standards-review`
- `alice` → matches meetings hosted by `Alice Chen` or `Alice Rodriguez`

The match is always a **prefix** match on the last word you type, so `dup` will **not** match `standup` — only words that start with `dup` would.

## Understanding the results

Each result row shows the meeting ID, the host's name, and a status badge:

| Badge | Meaning | Action |
|-------|---------|--------|
| **Join** (green) | Active, idle, or just created | Click to join the meeting |
| **Ended** (grey) | Meeting has ended | Read-only; cannot be joined |
| Other state (yellow) | Unusual state (e.g. `waiting`) | Varies |

Only meetings with the **Join** badge are clickable. Ended meetings are shown for reference.

### Opening a meeting

- **Click** a Join row → opens the meeting in the current tab
- **Cmd-click** (Mac) or **Ctrl-click** (Windows/Linux) → opens the meeting in a new tab
- Up to **20 results** are shown per query; narrow your search term if you don't see the meeting you want

## Privacy and access control

The search only returns meetings you are allowed to see:

- **Signed in:** you see meetings where you are the **host** or an **admitted participant**. You cannot discover meetings other people host that you've never joined.
- **Signed out (local dev):** the search falls back to a limited result set based on the meeting database; some hosted deployments return nothing until you sign in.

This filtering happens on the server side — there is no way for a user to bypass it by crafting a clever search term.

## Troubleshooting

| Symptom | Likely cause |
|---------|--------------|
| "No meetings found" but you know the meeting exists | You are not a participant on it, or it was created before search indexing was enabled |
| Search shows "Searching..." forever | Your network blocked the search backend; refresh and retry |
| Typing special characters (`*`, `:`, `(` …) returns nothing | Those characters are treated as literal text; the meeting ID probably doesn't contain them |
| Results don't update as you type | Close the modal with Esc and reopen it — the focus may have been lost |

If search remains broken, the system automatically falls back to a direct database lookup against the Meeting API, so you should still be able to find your meetings — just without the typeahead speed.

## See also

- [Meeting API](MEETING_API.md) — the backend that stores meetings and populates the search index
- [Meeting Ownership](MEETING_OWNERSHIP.md) — how meetings are created, admitted, and ended
