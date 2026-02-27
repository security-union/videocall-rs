# OAuth/OIDC Helm Configuration Guide

This guide covers configuring the `meeting-api` Helm chart for OAuth2/OIDC authentication with different identity providers.

## Overview

The Meeting API supports any OAuth2/OIDC-compliant identity provider. There are two configuration modes:

1. **OIDC Discovery** (recommended) — Set `OAUTH_ISSUER` and endpoints are auto-discovered
2. **Manual** — Set `OAUTH_AUTH_URL` and `OAUTH_TOKEN_URL` explicitly

Discovery is preferred because it also auto-populates JWKS and UserInfo endpoints, enabling full ID token signature verification.

---

## Kubernetes Secrets

OAuth credentials should be stored in a Kubernetes secret and not in Helm values. The secret name is referenced in the Helm values file.

### Create the secret

```bash
# For confidential clients (e.g. Google):
kubectl create secret generic oauth-credentials \
  --from-literal=client-id='YOUR_CLIENT_ID' \
  --from-literal=client-secret='YOUR_CLIENT_SECRET'

# For public clients (e.g. Okta with PKCE only):
kubectl create secret generic oauth-credentials \
  --from-literal=client-id='YOUR_CLIENT_ID'
```

> **Security:** Never put credentials in `values.yaml` or any file committed to git. Always use Kubernetes secrets.

---

## Provider Configuration

### Google

Google is a confidential client — a client secret is required.

**Helm values (`values.yaml`):**

```yaml
env:
  # ... other env vars ...

  - name: OAUTH_CLIENT_ID
    valueFrom:
      secretKeyRef:
        name: oauth-credentials
        key: client-id
  - name: OAUTH_SECRET
    valueFrom:
      secretKeyRef:
        name: oauth-credentials
        key: client-secret
  - name: OAUTH_ISSUER
    value: "https://accounts.google.com"
  - name: OAUTH_REDIRECT_URL
    value: "https://api.videocall.rs/login/callback"
  - name: OAUTH_SCOPES
    value: "openid email profile"
  - name: AFTER_LOGIN_URL
    value: "https://app.videocall.rs"
```

**What happens at startup:**
1. Discovery fetches `https://accounts.google.com/.well-known/openid-configuration`
2. Auto-populates:
   - `auth_url` → `https://accounts.google.com/o/oauth2/v2/auth`
   - `token_url` → `https://oauth2.googleapis.com/token`
   - `jwks_url` → `https://www.googleapis.com/oauth2/v3/certs`
   - `userinfo_url` → `https://openidconnect.googleapis.com/v1/userinfo`
3. ID tokens are verified using Google's JWKS keys (RS256)

**Google Cloud Console setup:**
1. Go to APIs & Services → Credentials
2. Create an OAuth 2.0 Client ID (Web application)
3. Add `https://api.videocall.rs/login/callback` as an Authorized redirect URI
4. Copy the Client ID and Client Secret into the Kubernetes secret

---

### Okta

Okta can be configured as either a confidential or public client. For browser-based apps using PKCE, a public client (no secret) is typical.

**Helm values (`values.yaml`) — public client (PKCE only):**

```yaml
env:
  # ... other env vars ...

  - name: OAUTH_CLIENT_ID
    valueFrom:
      secretKeyRef:
        name: oauth-credentials
        key: client-id
  # OAUTH_SECRET intentionally omitted — public client
  - name: OAUTH_ISSUER
    value: "https://your-org.okta.com"
  - name: OAUTH_REDIRECT_URL
    value: "https://api.videocall.rs/login/callback"
  - name: OAUTH_SCOPES
    value: "openid email profile"
  - name: AFTER_LOGIN_URL
    value: "https://app.videocall.rs"
```

**Helm values (`values.yaml`) — confidential client:**

```yaml
env:
  # ... other env vars ...

  - name: OAUTH_CLIENT_ID
    valueFrom:
      secretKeyRef:
        name: oauth-credentials
        key: client-id
  - name: OAUTH_SECRET
    valueFrom:
      secretKeyRef:
        name: oauth-credentials
        key: client-secret
  - name: OAUTH_ISSUER
    value: "https://your-org.okta.com"
  - name: OAUTH_REDIRECT_URL
    value: "https://api.videocall.rs/login/callback"
  - name: OAUTH_SCOPES
    value: "openid email profile"
  - name: AFTER_LOGIN_URL
    value: "https://app.videocall.rs"
```

**What happens at startup:**
1. Discovery fetches `https://your-org.okta.com/.well-known/openid-configuration`
2. Auto-populates:
   - `auth_url` → `https://your-org.okta.com/oauth2/v1/authorize`
   - `token_url` → `https://your-org.okta.com/oauth2/v1/token`
   - `jwks_url` → `https://your-org.okta.com/oauth2/v1/keys`
   - `userinfo_url` → `https://your-org.okta.com/oauth2/v1/userinfo`
3. ID tokens are verified using Okta's JWKS keys
4. Nonce is validated to prevent replay attacks

**Okta Admin Console setup:**
1. Go to Applications → Create App Integration
2. Select "OIDC - OpenID Connect" → "Web Application" (or "Single-Page Application" for public client)
3. Grant type: Authorization Code
4. Sign-in redirect URI: `https://api.videocall.rs/login/callback`
5. Assignments: Assign to the appropriate users/groups
6. Copy the Client ID (and Client Secret if confidential) into the Kubernetes secret

---

### Generic OIDC Provider

Any OIDC-compliant provider works with the discovery flow:

```yaml
env:
  - name: OAUTH_CLIENT_ID
    valueFrom:
      secretKeyRef:
        name: oauth-credentials
        key: client-id
  - name: OAUTH_SECRET
    valueFrom:
      secretKeyRef:
        name: oauth-credentials
        key: client-secret
        optional: true
  - name: OAUTH_ISSUER
    value: "https://your-provider.example.com"
  - name: OAUTH_REDIRECT_URL
    value: "https://api.videocall.rs/login/callback"
  - name: OAUTH_SCOPES
    value: "openid email profile"
  - name: AFTER_LOGIN_URL
    value: "https://app.videocall.rs"
```

The provider must serve a valid discovery document at `{OAUTH_ISSUER}/.well-known/openid-configuration`.

---

## Manual Configuration (No Discovery)

For providers that don't support OIDC Discovery, set endpoints manually:

```yaml
env:
  - name: OAUTH_CLIENT_ID
    valueFrom:
      secretKeyRef:
        name: oauth-credentials
        key: client-id
  - name: OAUTH_SECRET
    valueFrom:
      secretKeyRef:
        name: oauth-credentials
        key: client-secret
  # No OAUTH_ISSUER — manual mode
  - name: OAUTH_AUTH_URL
    value: "https://provider.example.com/authorize"
  - name: OAUTH_TOKEN_URL
    value: "https://provider.example.com/token"
  - name: OAUTH_JWKS_URL
    value: "https://provider.example.com/.well-known/jwks.json"
  - name: OAUTH_USERINFO_URL
    value: "https://provider.example.com/userinfo"
  - name: OAUTH_REDIRECT_URL
    value: "https://api.videocall.rs/login/callback"
  - name: OAUTH_SCOPES
    value: "openid email profile"
  - name: AFTER_LOGIN_URL
    value: "https://app.videocall.rs"
```

> **Note:** When `OAUTH_ISSUER` is not set, `OAUTH_AUTH_URL` and `OAUTH_TOKEN_URL` are required. JWKS-based signature verification is only available when `OAUTH_JWKS_URL` is set (either manually or via discovery).

---

## Environment Variable Reference

| Variable | Required | Default | Description |
|---|---|---|---|
| `OAUTH_CLIENT_ID` | Yes | — | OAuth client ID. OAuth is disabled if unset. |
| `OAUTH_SECRET` | No | — | Client secret. Omit for public clients (PKCE only). |
| `OAUTH_REDIRECT_URL` | Yes | — | Callback URL registered with the provider. |
| `OAUTH_ISSUER` | No | — | OIDC issuer URL. Enables discovery + JWT `iss` validation. |
| `OAUTH_AUTH_URL` | Cond. | — | Authorization endpoint. Required when `OAUTH_ISSUER` not set. |
| `OAUTH_TOKEN_URL` | Cond. | — | Token endpoint. Required when `OAUTH_ISSUER` not set. |
| `OAUTH_JWKS_URL` | No | — | JWKS endpoint. Overrides discovery. Enables ID token verification. |
| `OAUTH_USERINFO_URL` | No | — | UserInfo endpoint. Fallback when ID token lacks `email`. |
| `OAUTH_SCOPES` | No | `openid email profile` | Space-separated scopes. |
| `AFTER_LOGIN_URL` | No | `/` | Redirect after login. |

**Resolution order:**
1. If `OAUTH_ISSUER` is set → discover endpoints, then apply manual overrides on top
2. If `OAUTH_ISSUER` is not set → `OAUTH_AUTH_URL` + `OAUTH_TOKEN_URL` must be set manually

---

## Deploying

```bash
# 1. Create the secret (do this once per cluster/namespace)
kubectl create secret generic oauth-credentials \
  --from-literal=client-id='YOUR_CLIENT_ID' \
  --from-literal=client-secret='YOUR_CLIENT_SECRET'

# 2. Install or upgrade the chart
helm upgrade --install meeting-api helm/meeting-api/ \
  -f helm/global/us-east/meeting-api/values.yaml

# 3. Verify the pod starts and discovery succeeds
kubectl logs -l app=meeting-api | grep "OIDC discovery"
```

Expected log output on successful startup:
```
INFO Running OIDC discovery for issuer: https://accounts.google.com
INFO OIDC discovery complete: auth_url=https://accounts.google.com/o/oauth2/v2/auth, token_url=https://oauth2.googleapis.com/token, jwks_url=Some("https://www.googleapis.com/oauth2/v3/certs"), userinfo_url=Some("https://openidconnect.googleapis.com/v1/userinfo")
```

---

## Troubleshooting

### OIDC discovery failed

```
ERROR OIDC discovery failed: ...
```

- Verify the issuer URL is correct and reachable from the pod
- Check `curl https://your-issuer/.well-known/openid-configuration` from inside the cluster
- Some providers require a path suffix (e.g. `https://your-org.okta.com/oauth2/default`)

### JWT validation failed: InvalidIssuer

The `iss` claim in the ID token doesn't match `OAUTH_ISSUER`. Ensure the issuer URL matches exactly what the provider puts in its tokens (trailing slash matters).

### JWT validation failed: InvalidAudience

The `aud` claim doesn't match `OAUTH_CLIENT_ID`. Verify the client ID is correct.

### OAuth token exchange failed

- Check that `OAUTH_REDIRECT_URL` matches exactly what's registered in the provider's console
- For confidential clients, ensure `OAUTH_SECRET` is set correctly
- Check pod logs for the HTTP status and response body

### Email not available from ID token or UserInfo

The provider's ID token didn't include an `email` claim, and either `OAUTH_USERINFO_URL` is not configured or the UserInfo endpoint also didn't return an email. Ensure the `email` scope is included in `OAUTH_SCOPES` and that the user has an email in the provider.

### JWKS key not found for kid

The ID token references a key ID that isn't in the provider's JWKS. This can happen if keys were recently rotated. The cache refreshes every 5 minutes — wait and retry. If persistent, verify `OAUTH_JWKS_URL` points to the correct endpoint.
