# Postbird

Postbird is a native Linux Gmail client built with Rust, GTK 4, and libadwaita.
It talks directly to Gmail via IMAP/SMTP with an app password or GNOME Online
Accounts. There is no Postbird server or Postbird OAuth client; mail data remains
on your computer apart from requests to Google.

## Features

- Multiple Google accounts with instant account switching
- Gmail app-password access over TLS-protected IMAP/SMTP
- GNOME Online Accounts sign-in over Gmail IMAP/SMTP and XOAUTH2
- App passwords stored in the Linux keyring; GOA tokens are never stored by Postbird
- Inbox, Starred, Sent, Drafts, and Trash views
- Favourite folders across accounts, with custom names and drag-to-reorder
- Gmail search and offline mailbox cache
- Live Inbox updates through IMAP IDLE with either sign-in method
- Read, archive, star, mark unread, and trash messages
- Compose, reply, save drafts, and send messages
- Per-account sending aliases, selectable in the compose From field
- Larger WYSIWYG message editor with HTML replies/forwards, formatting, file attachments, and pasted inline images
- Local, per-account recipient suggestions matching names or email addresses

## Gmail app-password setup

Enable 2-Step Verification in your Google Account, then create an app password.
Launch Postbird, press **+**, choose **Gmail app password**, and enter the full
mail address and generated password. Postbird checks both IMAP and SMTP before
saving the account. Do not enter your normal Google Account password.

App-password accounts use `imap.gmail.com:993` for mail and `smtp.gmail.com:465`
for sending, both with TLS. Postbird uses Gmail's IMAP extensions for thread IDs,
labels, and search. The IMAP/SMTP transport uses Python 3's standard library,
so Python 3 must be installed at runtime (3.14+ for live updates). Some managed
Workspace, Advanced Protection, and security-key-only accounts cannot create
app passwords.

## GNOME Online Accounts setup (experimental)

Add a Google account in GNOME Online Accounts and enable Mail for it. In
Postbird, press **+**, choose **GNOME Online Accounts**, select the account,
and connect. Postbird checks IMAP and SMTP before adding it. GOA retains the
sign-in credentials; Postbird stores only the selected GOA account ID and asks
GOA for a short-lived access token when it connects. Removing it from Postbird
does not remove it from GNOME Online Accounts.

If GOA is unavailable or no Google account has Mail enabled, Postbird explains
what to set up and offers to open Online Accounts (when its settings app is
installed), retry, or use a Gmail app password instead.

This path requires `gnome-online-accounts` and Python's `gi` bindings for GOA
at runtime. It is a technical integration test, not an established exemption
from Google's restricted-scope verification or CASA requirements. GNOME asks
third-party apps to coordinate with its maintainers before shipping use of
its account profiles.

## Mail updates

Postbird keeps one IMAP IDLE connection open per account while running. Inbox
changes trigger a refresh; bursts are combined, with at most one IDLE-triggered
refresh per account every ten seconds. The listener reconnects after network
interruptions and obtains a fresh GOA token when reconnecting.

The former one-minute background checks now run every 15 minutes as a fallback
for missed events and favourite folders outside the Inbox. Manual refresh still
works immediately. Python versions older than 3.14, or servers without IDLE,
use these periodic checks. The IDLE connection is renewed every 20 minutes
without fetching messages. New message bodies and read-status changes still
require normal IMAP requests.

Switching accounts or folders reuses a complete cache checked within the last
15 minutes when no changes are pending. Empty folders are cached too. IDLE
events, disconnects, and local mail actions invalidate the relevant snapshots;
an inactive account catches up when opened. Each application session performs
an initial reconciliation, and Refresh always checks Gmail.

Folder navigation no longer automatically opens or marks the first message
read. Small conversation refreshes share one connection, and recent/unread
folder listings share a connection. Unread-count checks avoid downloading the
visible folder again while it is already syncing, and successful mark-as-read
operations use their local count adjustments without an immediate extra check.

## Composing messages

Replies include the previous message as an editable quotation. Replies, forwards,
and reopened drafts retain HTML formatting, tables, links, and embedded images.
The toolbar supports emphasis, lists, undo, and redo; paste accepts formatted text
and screenshots. Remote images are blocked in the editor, while their URLs remain
in the outgoing HTML.

Postbird indexes names and addresses from From, Reply-To, To, and Cc headers in
locally synced Inbox messages on background workers. Existing cached messages are
indexed at startup, and Inbox sync updates the index. Type a name or email address
in To, Cc, or Bcc to select a suggestion. Contacts remain local, are separate for
each account, and are removed when that account is removed from Postbird.

## Sending aliases

In **Settings → Accounts**, select **Aliases** beside an account. Enter one
email address per line and save. Choose the address in the compose dialog's
**From** dropdown; drafts retain that selection. Sending uses the original
account's sign-in credentials.

Enable each address in Gmail's **Send mail as** settings first; see
[Google's alias setup instructions](https://support.google.com/mail/answer/22370).
For example, add `kent@otron.com` as an alias of `kent@otron.net`, then select
`kent@otron.com` when composing. Delete its line in Aliases to remove it.

## Build and run

On Arch Linux:

```sh
sudo pacman -S --needed rust gtk4 libadwaita webkitgtk-6.0 librsvg
cargo run
```

For the optional GOA connection, also install `gnome-online-accounts` and
`python-gobject`.

Validate the project:

```sh
cargo test
cargo clippy --all-targets -- -D warnings
python3 -m unittest discover -s scripts -p 'test_*.py'
```

## Install for the current user

```sh
./scripts/install.sh
```

This installs the release binary under `~/.local/bin` and registers Postbird in
the desktop application menu. The installer uses `rsvg-convert` from `librsvg`
to prepare the Lucide icons in GTK's symbolic PNG format at several sizes.

## Local data

- Account list and UI preferences: `~/.config/postbird/`
- Offline message cache: `~/.local/share/postbird/mail.db`
- App passwords: Linux keyring, service `io.github.postbird.Mail`

Removing an account in Postbird also clears that account's cached mail. It does
not remove other accounts or delete mail from Gmail. Older Postbird OAuth
accounts are skipped when loading accounts; re-add them through GOA or an app
password. Postbird does not automatically delete old OAuth client files or
keyring entries during this upgrade.

Postbird never stores app passwords in plaintext files.
