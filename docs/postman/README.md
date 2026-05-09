# Meeting API - Postman Collection

This directory contains Postman collection and environment files for testing the Meeting API.

## Files

| File | Description |
|------|-------------|
| `Meeting_API_Collection.postman_collection.json` | Complete API collection with all endpoints |
| `Local_Development.postman_environment.json` | Environment for local development (port 8081) |
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

3. Obtain a JWT session token. Choose one method:

   **Option A — Dev auto-login (no OAuth required, recommended for local testing)**

   Requires OAuth disabled (`OAUTH_CLIENT_ID` unset) and `DEV_USER=dev@test.local:Dev User` set in your `.env` file. Then run:
   ```bash
   curl -s -o /dev/null -c /tmp/cookies.txt http://localhost:8081/api/v1/dev/auto-login
   awk '$6 == "session" {print $7}' /tmp/cookies.txt
   ```
   Paste the printed JWT into the `session_token` variable in Postman.

   **Option B — OAuth browser flow**
   - Visit `http://localhost:8081/login` and complete the OAuth login
   - Copy the `session` cookie value from your browser's DevTools
   - Paste it into the `session_token` environment variable in Postman

4. Update variables if needed:
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

All endpoints require a JWT session token passed via the `Authorization` header:

```
Authorization: Bearer <session_token>
```

To obtain a session token for local development, use `DEV_USER` auto-login (see [Local Development Setup](#local-development-setup) above). For production or OAuth-enabled environments, complete the browser OAuth flow and copy the `session` cookie from DevTools.

For the attendee workflow, obtain a second token from a different user session and set it as `participant_session_token`.

## Variables Reference

| Variable | Description | Example |
|----------|-------------|---------|
| `baseUrl` | Meeting API base URL | `http://localhost:8081` |
| `session_token` | JWT session token (host) | `eyJ...` |
| `participant_session_token` | JWT session token (attendee) | `eyJ...` |
| `email` | Your email (for reference) | `host@example.com` |
| `meeting_id` | Meeting identifier | `my-meeting` |
| `participant_email` | Attendee email | `attendee@example.com` |
| `display_name` | Display name in meeting | `Test User` |

## Testing Workflow

### Test as Host
1. Set `session_token` in your environment
2. Run **"1. Host - Create and Join"** from Workflows
3. Copy the `room_token` from response
4. Connect to the Media Server: `ws://localhost:8080/lobby?token=<room_token>`

### Test as Attendee (requires two sessions)
1. In Session 1 (Host): Set `session_token`, run **"1. Host - Create and Join"**
2. In Session 2 (Attendee): Set `participant_session_token` from a different user's login
3. In Session 2: Run **"2. Attendee - Request to Join"**
4. In Session 1: Run **"3. Host - Check Waiting Room"**
5. In Session 1: Run **"4. Host - Admit Attendee"**
6. In Session 2: Run **"5. Attendee - Poll for Token"**
7. Copy `room_token` and connect to the Media Server
