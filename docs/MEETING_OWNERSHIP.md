# Meeting Ownership and Workflow

This document describes the meeting ownership model, lifecycle, and user workflows in videocall.rs.

## Table of Contents

- [Overview](#overview)
- [Meeting Ownership](#meeting-ownership)
- [Meeting Lifecycle](#meeting-lifecycle)
- [User Interface Workflows](#user-interface-workflows)
- [My Meetings List](#my-meetings-list)
- [Host Identification](#host-identification)
- [Waiting Room](#waiting-room)
- [Database Schema](#database-schema)

---

## Overview

videocall.rs implements a meeting ownership model where:

- **Every meeting has an owner** (the user who created it)
- **The owner is identified by their email address** (from OAuth authentication)
- **Ownership persists** even after the meeting ends
- **Only owners can delete their meetings**
- **The "My Meetings" list shows only meetings owned by the current user**

---

## Meeting Ownership

### How Ownership is Assigned

1. **Explicit Creation**: When a user calls `POST /api/v1/meetings`, they become the owner
2. **Implicit Creation**: When a user joins a meeting that doesn't exist (`POST /api/v1/meetings/{id}/join`), the meeting is created and they become the owner

### Owner Identification

- Ownership is stored in the `creator_id` field of the `meetings` table
- The `creator_id` contains the owner's email address
- This is set at meeting creation time and never changes

### Owner vs Host Display Name

There's an important distinction between:

- **Owner (creator_id)**: The email address of the user who owns the meeting (permanent, used for authorization)
- **Host Display Name**: The display name shown in the UI for the host (dynamic, looked up from participants)

The host display name is resolved by looking up the owner's email in the `meeting_participants` table to find their chosen display name for that meeting.

---

## Meeting Lifecycle

### Meeting States

| State | Description |
|-------|-------------|
| `idle` | Meeting created but owner hasn't joined yet |
| `active` | Owner has joined, meeting is in progress |
| `ended` | Meeting has ended (all participants left) |

### State Transitions

```
┌─────────────────────────────────────────────────────────────┐
│                                                             │
│   [Create Meeting]                                          │
│         │                                                   │
│         ▼                                                   │
│      ┌──────┐                                               │
│      │ idle │ ◄─────────────────────────────────────┐       │
│      └──┬───┘                                       │       │
│         │                                           │       │
│         │ [Owner joins]                             │       │
│         ▼                                           │       │
│     ┌────────┐                                      │       │
│     │ active │ ◄───────────────────────┐            │       │
│     └───┬────┘                         │            │       │
│         │                              │            │       │
│         │ [All participants leave]     │ [Rejoin]   │       │
│         ▼                              │            │       │
│     ┌───────┐                          │            │       │
│     │ ended │ ─────────────────────────┘            │       │
│     └───┬───┘                                       │       │
│         │                                           │       │
│         │ [Owner deletes]                           │       │
│         ▼                                           │       │
│     ┌─────────┐                                     │       │
│     │ deleted │ (soft delete: deleted_at set)       │       │
│     └─────────┘                                     │       │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

### Soft Delete vs Hard Delete

Meetings use **soft deletion**:
- When deleted, `deleted_at` timestamp is set (not physically removed)
- Soft-deleted meetings don't appear in "My Meetings"
- The same meeting ID can be reused after deletion (partial unique index)

---

## User Interface Workflows

### Owner Starting a Meeting

1. Owner navigates to a meeting URL (e.g., `/meeting/my-standup`)
2. Owner enters their display name
3. Owner clicks **"Start Meeting"** (button shows "Start" for owners)
4. Meeting is created (if new) or activated (if existing)
5. Owner enters the meeting room

### Participant Joining a Meeting

1. Participant navigates to meeting URL
2. Participant enters their display name
3. Participant clicks **"Join Meeting"** (button shows "Join" for non-owners)
4. If meeting is active:
   - Participant enters the waiting room
   - Host admits or rejects participant
   - If admitted, participant auto-joins the meeting
5. If meeting doesn't exist:
   - Meeting is created with participant as owner
   - Participant becomes the host

### Owner Deleting a Meeting

1. Owner goes to the home page
2. Owner expands "My Meetings" section
3. Owner clicks the delete icon (trash) next to their meeting
4. Confirmation dialog appears
5. Meeting is soft-deleted and removed from the list

---

## My Meetings List

The "My Meetings" list on the home page shows all meetings owned by the current user.

### Features

- **Filtered by Owner**: Only shows meetings where `creator_id` matches the user's email
- **Includes Ended Meetings**: Ended meetings remain visible until deleted
- **Excludes Deleted Meetings**: Soft-deleted meetings are hidden
- **Shows Meeting Status**: Active, idle, or ended state displayed
- **Delete Button**: Owners can delete their meetings directly from the list

### Meeting Summary Information

Each meeting in the list shows:
- Meeting ID (clickable to join)
- Current state (active/idle/ended)
- Host email
- Participant count
- Password indicator (if password-protected)
- Delete button (for owner only)

### API Endpoint

```
GET /api/v1/meetings?limit=20&offset=0
```

Returns only meetings owned by the authenticated user.

---

## Host Identification

### In the Meeting Room

The host is identified in the UI with:
- **(Host)** text displayed after the host's display name
- Tooltip showing "Host: [name]" on hover

### How Host Display Name is Resolved

1. Meeting stores `creator_id` (owner's email)
2. When fetching meeting info, the system looks up the owner's display name from `meeting_participants`
3. This display name is used to show the "(Host)" indicator in the UI

### Visual Indicators

| Location | Host Indicator |
|----------|----------------|
| Video tile | "(Host)" after name |
| Peer list | "(Host)" after name |
| Hover tooltip | "Host: [display name]" |

---

## Waiting Room

### Overview

The waiting room provides controlled access to meetings:
- Non-owners enter the waiting room when joining an active meeting
- Admitted participants can manage the waiting room (not just the host)
- Participants poll for status changes while waiting

### Participant Management

Any admitted participant can:
- View the waiting room list
- Admit individual participants
- Admit all waiting participants at once
- Reject participants

### Auto-Join Behavior

When a participant is admitted from the waiting room:
1. Their status changes to "admitted"
2. The UI detects this via polling
3. The participant automatically joins the meeting (no "Join" button click needed)

### API Endpoints

| Endpoint | Description |
|----------|-------------|
| `GET /meetings/{id}/waiting` | List waiting participants |
| `POST /meetings/{id}/admit` | Admit one participant |
| `POST /meetings/{id}/admit-all` | Admit all waiting |
| `POST /meetings/{id}/reject` | Reject a participant |
| `GET /meetings/{id}/status` | Check your own status (for polling) |

---

## Database Schema

### meetings Table

| Column | Type | Description |
|--------|------|-------------|
| id | SERIAL | Primary key |
| room_id | VARCHAR(255) | Meeting identifier (unique among non-deleted) |
| creator_id | VARCHAR(255) | Owner's email address |
| state | VARCHAR(50) | `idle`, `active`, `ended` |
| password_hash | VARCHAR(255) | Argon2 hashed password (optional) |
| started_at | TIMESTAMPTZ | When meeting was created |
| ended_at | TIMESTAMPTZ | When meeting ended |
| deleted_at | TIMESTAMPTZ | Soft delete timestamp |
| host_display_name | VARCHAR(255) | Cached host display name |

### meeting_participants Table

| Column | Type | Description |
|--------|------|-------------|
| id | SERIAL | Primary key |
| meeting_id | INTEGER | Foreign key to meetings.id |
| email | VARCHAR(255) | Participant's email |
| display_name | VARCHAR(255) | Participant's chosen display name |
| status | VARCHAR(50) | `waiting`, `admitted`, `rejected`, `left` |
| is_host | BOOLEAN | True if this is the meeting owner |
| joined_at | TIMESTAMPTZ | When joined/entered waiting room |
| admitted_at | TIMESTAMPTZ | When admitted to meeting |
| left_at | TIMESTAMPTZ | When left the meeting |

### Key Indexes

```sql
-- Unique meeting IDs among non-deleted meetings
CREATE UNIQUE INDEX idx_meetings_room_id_unique_active
ON meetings(room_id) WHERE deleted_at IS NULL;

-- Fast lookup by owner
CREATE INDEX idx_meetings_creator_id ON meetings(creator_id);

-- Fast lookup by state
CREATE INDEX idx_meetings_state ON meetings(state);
```

---

## Related Documentation

- [Meeting API Documentation](MEETING_API.md) - Detailed API endpoint reference
- [Architecture Document](../ARCHITECTURE.md) - System architecture overview
