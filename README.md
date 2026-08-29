# Postbird

Postbird is a native Linux Gmail client built with Rust, GTK 4, and libadwaita.
It talks directly to Google's Gmail API; there is no Postbird server and Gmail
data remains on your computer apart from API requests to Google.

## Features

- Multiple Google accounts with instant account switching
- Browser-based OAuth using Google's loopback flow and PKCE
- Refresh tokens stored in the Linux keyring
- Inbox, Starred, Sent, Drafts, and Trash views
- Gmail search, pagination, and offline inbox cache
- Read, archive, star, mark unread, and trash messages
- Compose, reply, save drafts, and send messages

## Google setup

Google requires credentials from a Google Cloud project:

1. Open the [Google Cloud Console](https://console.cloud.google.com/).
2. Create or select a project and enable the **Gmail API**.
3. Configure the OAuth consent screen. During development, add each Gmail
   address you plan to use as a test user.
4. Create an OAuth client with application type **Desktop app**.
5. Download its JSON credentials file.
6. Launch Postbird, press the **+** button, and choose the downloaded JSON file.

Postbird copies the credentials to its user configuration directory. Each
account signs in through the system browser. The browser returns authorization
to a temporary listener bound only to `127.0.0.1`.

The app requests `gmail.modify`, which Google classifies as a restricted scope.
Personal/test use works with consent-screen test users. Public distribution
requires completing Google's OAuth verification process.

## Build and run

On Arch Linux:

```sh
sudo pacman -S --needed rust gtk4 libadwaita webkitgtk-6.0
cargo run
```

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

- Account list and imported OAuth client: `~/.config/postbird/`
- Offline message cache: `~/.local/share/postbird/mail.db`
- Google tokens: Linux keyring, service `io.github.postbird.Mail`

Postbird never stores Google refresh tokens in plaintext files.
