# Use Arr from ChatGPT on your phone or any device

ChatGPT talks to Arr from OpenAI's servers, so Arr needs an address on the public internet
and a sign-in that keeps everyone else out. Arr uses:

- **ngrok** for the public HTTPS address. The free plan gives you one permanent domain, such
  as `quiet-river-1234.ngrok-free.app`, and the tunnel runs next to Arr on your server. You
  don't open any ports on your router.
- **Google sign-in** to decide who gets in. Arr asks Google who you are and lets in only the
  Google accounts you list.

You set this up once, about 15 minutes. After that, Arr works in ChatGPT on the web, on your
phone and on every other device signed in to your ChatGPT account.

## 1. Get an ngrok domain and authtoken

1. Sign up at <https://dashboard.ngrok.com/signup> (free).
2. Open **Your Authtoken** and copy the token.
3. Open **Domains**. Copy your free domain, or create one if the list is empty.

## 2. Create a Google OAuth client

You need a Google account for this; the client costs nothing.

1. Open <https://console.cloud.google.com/> and create a project, for example `arr`.
2. Open **Google Auth Platform** (also listed as **APIs & Services > OAuth consent screen**)
   and choose **Get started**. Name the app `Arr`, pick your email as the support address,
   choose **External** as the audience, and finish.
3. Under **Audience**, leave the app in **Testing** and add each Google account that should use
   Arr as a **test user**.
4. Under **Clients**, choose **Create client**:
   - Application type: **Web application**
   - Authorized redirect URI: `https://YOUR-NGROK-DOMAIN/auth/callback`
5. Copy the **client ID** and **client secret**.

Arr asks Google only for your email address, so the app needs no review by Google.

## 3. Turn on remote access

On the server that runs Arr, run the installer again. It keeps your config and adds remote
access:

```sh
curl -fsSL https://raw.githubusercontent.com/newtonharry/Arr/main/install.sh | sh -s -- --remote
```

On Windows, open PowerShell (Start menu, type "PowerShell") and paste:

```powershell
$env:ARR_REMOTE = '1'; irm https://raw.githubusercontent.com/newtonharry/Arr/main/install.ps1 | iex
```

A first install offers this at the end on every platform, so you can just answer yes there.
The installer walks you through steps 1 and 2 as it goes: it opens each ngrok and Google page
in your browser (when you're at the computer, not over SSH), tells you what to copy, and the
secrets you paste stay hidden.

It asks for the five values from steps 1 and 2: the ngrok authtoken and domain, the Google
client ID and secret, and the Google accounts allowed in. Then it:

- installs the ngrok agent if you don't have it;
- stores the authtoken and client secret in private files in Arr's state folder (mode 600 on
  Linux and macOS; on Windows the folder only your account can open);
- adds the sign-in settings to `config.toml` and checks them with `arr check`, restoring your
  old config if the check fails;
- starts the tunnel next to Arr, and again whenever the computer starts or you log in:
  `arr-tunnel` on Linux, a launchd agent on macOS, and on Windows an **Arr tunnel** task that
  runs at log-on without a window and restarts ngrok if it stops;
- confirms that `https://YOUR-NGROK-DOMAIN` reaches Arr.

Running the installer again keeps your remote access as it is (to change it, remove the
`[mcp.oauth]` section and the `allowed_hosts` line from `config.toml` first). The uninstaller removes the tunnel with Arr.

## 4. Add Arr to ChatGPT

Do this once, on <https://chatgpt.com> in a browser:

1. Open **Settings > Apps & Connectors > Advanced** and turn on **Developer mode**.
2. Back in **Apps & Connectors**, choose **Create**:
   - Name: `Arr`
   - MCP server URL: `https://YOUR-NGROK-DOMAIN/mcp`
   - Authentication: **OAuth**
3. Choose **Create**. A window opens on Arr's sign-in page:
   - ngrok's free plan first shows a "You are about to visit" page; choose **Visit Site**;
   - approve ChatGPT on Arr's page;
   - pick one of the Google accounts you allowed.
4. Start a new chat and ask "what's in my library?"

The connector now works in the ChatGPT app on your phone and tablet too. OpenAI renames these
menus from time to time; if you can't find **Developer mode**, search ChatGPT's help for
"custom connectors". Custom connectors need a ChatGPT plan that offers them.

Each allowed person signs in once from their own ChatGPT account. To remove someone, delete
their address from `allowed_emails` in `config.toml` and restart Arr; their access stops
immediately.

## Let an AI agent set it up

An agent with shell access to your server can do step 3 for you. Steps 1, 2 and 4 need your own
ngrok, Google and ChatGPT logins, so do those yourself (or give an agent with a browser access
to them), then hand the agent these values. Pass secrets as environment variables so they
don't appear in the process list:

```sh
export ARR_NGROK_TOKEN='...' ARR_GOOGLE_CLIENT_SECRET='...'
curl -fsSL https://raw.githubusercontent.com/newtonharry/Arr/main/install.sh | sh -s -- --yes \
  --ngrok-domain quiet-river-1234.ngrok-free.app \
  --google-client-id 1234567890-abc.apps.googleusercontent.com \
  --allow-email you@gmail.com --allow-email partner@gmail.com
```

With `--yes` the script never prompts. A missing value stops it with an `error:` line naming
the flag to add. Success ends with "Arr is reachable at https://..." and the ChatGPT steps.

On Windows every value is an environment variable (`irm | iex` can't take arguments):

```powershell
$env:ARR_YES = '1'; $env:ARR_REMOTE = '1'
$env:ARR_NGROK_TOKEN = '...'; $env:ARR_GOOGLE_CLIENT_SECRET = '...'
$env:ARR_NGROK_DOMAIN = 'quiet-river-1234.ngrok-free.app'
$env:ARR_GOOGLE_CLIENT_ID = '1234567890-abc.apps.googleusercontent.com'
$env:ARR_ALLOW_EMAILS = 'you@gmail.com, partner@gmail.com'
irm https://raw.githubusercontent.com/newtonharry/Arr/main/install.ps1 | iex
```

A missing value stops it with an `error:` line naming the variable to set.

## Troubleshooting

- **"doesn't reach Arr yet".** Check the tunnel log (`journalctl -u arr-tunnel`, or
  `journalctl --user -u arr-tunnel` for a user install; `tunnel.log` in the state folder on
  macOS, and `%LOCALAPPDATA%\Arr\tunnel.log` on Windows). An `ERR_NGROK_...` code there names
  the problem, often a wrong authtoken or a domain that belongs to another account.
- **Windows: the tunnel doesn't start.** Some antivirus programs, including Microsoft
  Defender at times, flag ngrok and remove it. Check Windows Security's protection history,
  allow `ngrok.exe` in `%LOCALAPPDATA%\Programs\Arr`, then run the installer again with
  `$env:ARR_REMOTE = '1'`. Task Scheduler shows the **Arr tunnel** task and its last result.
- **Google says "redirect_uri_mismatch".** The redirect URI in your Google client must be
  exactly `https://YOUR-NGROK-DOMAIN/auth/callback`.
- **Google says the app is blocked or you're not a tester.** Add that account as a test user
  under **Audience**.
- **Arr says access denied after Google.** The account isn't in `allowed_emails`.
- **Your own domain or tunnel.** Any HTTPS reverse proxy works instead of ngrok: list its host in
  `[mcp] allowed_hosts` and set `[mcp.oauth] public_url`. [MCP.md](MCP.md) documents every
  setting.
