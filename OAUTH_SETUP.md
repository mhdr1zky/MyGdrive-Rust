# OAuth setup for MyGdrive Rust

Each user supplies their own OAuth client. The repository and `.deb` do not include developer client JSON, account tokens, or Google Cloud project configuration.

## 1. Enable the Google Drive API

Open [Google Cloud Console](https://console.cloud.google.com/), create or select a project, and enable **Google Drive API** under **APIs & Services → Library**.

## 2. Configure consent

Set the application name, support email, audience, and test users in **Google Auth Platform**. For personal use with an **External** audience in **Testing**, add your account as a test user. Console labels can change; consult [Google's consent configuration guide](https://developers.google.com/workspace/guides/configure-oauth-consent).

| CLI option | Scope | Access |
| --- | --- | --- |
| `--scope drive` | `https://www.googleapis.com/auth/drive` | Read and manage accessible Drive files, including existing files |
| `--scope drive-file` | `https://www.googleapis.com/auth/drive.file` | Files created or authorized for the application; existing files are not automatically included |

Choosing a subfolder does not narrow OAuth permissions. Existing-file sync generally requires `drive`; a Google Picker authorization flow is not implemented. See [official scope classifications](https://developers.google.com/workspace/drive/api/guides/api-specific-auth).

## 3. Create a Desktop app client

Under **Clients**, create an OAuth client of type **Desktop app** and download its JSON. It must contain an `installed` section, not `web`. Store it privately:

```bash
chmod 600 "$HOME/Downloads/oauth-client.json"
```

The client ID identifies the application; it is not an account token. Client JSON cannot be used as `access.token`. Login uses the system browser and a loopback callback on a random local port, following [desktop OAuth](https://developers.google.com/identity/protocols/oauth2/native-app).

## 4. Sign in

In the dashboard, open **Accounts and folders → Connect a new account…**, select the client JSON, and complete browser consent.

```bash
mygdrive-sync --state "$HOME/.local/share/mygdrive-cli" --account default \
  login --client-json "$HOME/Downloads/oauth-client.json" --scope drive
```

Use the same state and account label for synchronization. Tokens are stored privately in `STATE/oauth/` and refreshed automatically, but are not OS-keyring encrypted.

If a callback expires or the browser reports connection refused, start a new login; old callback URLs cannot be reused. CLI `--timeout-seconds 1800` allows 30 minutes; `--no-browser` lets you open the authorization URL manually on the same computer.

## 5. Usage limits

Under [Google's token expiration policy](https://developers.google.com/identity/protocols/oauth2#expiration), refresh tokens for External/Testing projects with Drive scopes generally expire after seven days. Sign-in may need renewal. Public distribution must meet Google's applicable publishing and verification requirements.

Never publish client JSON, authorization codes, access/refresh tokens, or state directories. `.gitignore` excludes common credential filenames; still review commits before publishing.
