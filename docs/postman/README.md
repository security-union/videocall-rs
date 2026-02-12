# Meeting API - Postman Collection

This directory contains Postman collection and environment files for testing the Meeting API.

## Files

| File | Description |
|------|-------------|
| `Meeting_API_Collection.postman_collection.json` | Complete API collection with all endpoints |
| `Local_Development.postman_environment.json` | Environment for local development (port 8080) |
| `Production.postman_environment.json` | Environment template for production |

## Import Instructions

1. Open Postman
2. Click **Import** (top-left)
3. Drag and drop or select the files:
   - `Meeting_API_Collection.postman_collection.json`
   - `Local_Development.postman_environment.json` (or Production)
4. Select the environment from the dropdown (top-right)

## Local Development Setup

1. Start the backend services:
   ```bash
   make dev
   ```

2. Select **Local Development** environment in Postman

3. Update variables if needed:
   - `email`: Your test email
   - `meeting_id`: Meeting ID to test with

## Collection Structure

### Meetings
- **List My Meetings** - `GET /api/v1/meetings`
- **Create Meeting** - `POST /api/v1/meetings`
- **Get Meeting Info** - `GET /api/v1/meetings/{meeting_id}`
- **Delete Meeting** - `DELETE /api/v1/meetings/{meeting_id}`

### Join & Leave
- **Join Meeting** - `POST /api/v1/meetings/{meeting_id}/join`
- **Get My Status** - `GET /api/v1/meetings/{meeting_id}/status`
- **Leave Meeting** - `POST /api/v1/meetings/{meeting_id}/leave`

### Waiting Room
- **Get Waiting Room** - `GET /api/v1/meetings/{meeting_id}/waiting`
- **Admit Participant** - `POST /api/v1/meetings/{meeting_id}/admit`
- **Admit All Participants** - `POST /api/v1/meetings/{meeting_id}/admit-all`
- **Reject Participant** - `POST /api/v1/meetings/{meeting_id}/reject`

### Participants
- **Get Participants** - `GET /api/v1/meetings/{meeting_id}/participants`

### Workflows
Step-by-step requests demonstrating a complete meeting flow:
1. Host creates and joins meeting
2. Attendee requests to join
3. Host checks waiting room
4. Host admits attendee
5. Attendee polls for room token

## Authentication

### Local Development (port 8080)
Uses email cookies. The collection is pre-configured with:
```
Cookie: email={{email}}
```

### Production (port 8081)
Uses JWT session tokens. For production:
1. Complete OAuth login in browser
2. Copy the `session` cookie value
3. Update requests to use either:
   - `Cookie: session={{session_token}}`
   - `Authorization: Bearer {{session_token}}`

## Variables Reference

| Variable | Description | Example |
|----------|-------------|---------|
| `baseUrl` | API base URL | `http://localhost:8080` |
| `email` | Your email (host) | `host@example.com` |
| `meeting_id` | Meeting identifier | `my-meeting` |
| `participant_email` | Attendee email | `attendee@example.com` |
| `display_name` | Display name in meeting | `Test User` |
| `session_token` | JWT session token (production) | `eyJ...` |

## Testing Workflow

### Test as Host
1. Run **"1. Host - Create and Join"** from Workflows
2. Copy the `room_token` from response
3. Connect to WebSocket: `ws://localhost:8080/lobby?token=<room_token>`

### Test as Attendee (requires two sessions)
1. In Session 1 (Host): Run **"1. Host - Create and Join"**
2. In Session 2 (Attendee): Change `email` variable to attendee email
3. In Session 2: Run **"2. Attendee - Request to Join"**
4. In Session 1: Run **"3. Host - Check Waiting Room"**
5. In Session 1: Run **"4. Host - Admit Attendee"**
6. In Session 2: Run **"5. Attendee - Poll for Token"**
7. Copy `room_token` and connect to Media Server
