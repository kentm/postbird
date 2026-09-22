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
- Read, archive, star, mark unread, and trash messages
- Compose, reply, save drafts, and send messages

## Gmail app-password setup

Enable 2-Step Verification in your Google Account, then create an app password.
Launch Postbird, press **+**, choose **Gmail app password**, and enter the full
mail address and generated password. Postbird checks both IMAP and SMTP before
saving the account. Do not enter your normal Google Account password.

App-password accounts use `imap.gmail.com:993` for mail and `smtp.gmail.com:465`
for sending, both with TLS. Postbird uses Gmail's IMAP extensions for thread IDs,
labels, and search. The IMAP/SMTP transport uses Python 3's standard library,
so Python 3 must be installed at runtime. Some managed Workspace, Advanced
Protection, and security-key-only accounts cannot create app passwords.

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

## Check an app password without adding an account

You can separately test whether your Gmail account accepts an app password.
This standalone probe does not add an account
to Postbird, save the password, or send mail. It logs in to Gmail IMAP, opens
the Inbox read-only, and logs in to Gmail SMTP.

Enable Google 2-Step Verification, create an app password in your Google Account,
then run:

```sh
python3 scripts/gmail_app_password_probe.py
```

Enter your full email address and the generated app password when prompted.
Some managed Workspace and Advanced Protection accounts cannot use app passwords.
Do not enter your normal Google Account password. The probe uses Python's standard
library and stores no credentials; the secret remains in process memory only
until the probe exits.

## Build and run

On Arch Linux:

```sh
sudo pacman -S --needed rust gtk4 libadwaita webkitgtk-6.0
cargo run
```

For the optional GOA connection, also install `gnome-online-accounts` and
`python-gobject`.

Validate the project:

```sh
cargo test
cargo clippy --all-targets -- -D warnings
```

## Install for the current user

```sh
./scripts/install.sh
```

This installs the release binary under `~/.local/bin` and registers Postbird in
the desktop application menu.

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
