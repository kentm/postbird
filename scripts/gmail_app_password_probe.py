#!/usr/bin/env python3
"""Read-only Gmail IMAP and login-only SMTP app-password smoke test.

This standalone probe does not change Postbird's account configuration.
"""

import getpass
import imaplib
import smtplib
import ssl
import sys


TIMEOUT_SECONDS = 15


def probe_imap(address: str, password: str) -> int:
    with imaplib.IMAP4_SSL(
        "imap.gmail.com", 993, ssl_context=ssl.create_default_context(), timeout=TIMEOUT_SECONDS
    ) as connection:
        connection.login(address, password)
        status, data = connection.select("INBOX", readonly=True)
        if status != "OK" or not data or not data[0]:
            raise imaplib.IMAP4.error("Could not open Inbox read-only")
        return int(data[0])


def probe_smtp(address: str, password: str) -> None:
    with smtplib.SMTP_SSL(
        "smtp.gmail.com", 465, context=ssl.create_default_context(), timeout=TIMEOUT_SECONDS
    ) as connection:
        connection.ehlo()
        connection.login(address, password)


def main() -> int:
    print("Gmail app-password probe: authenticates to IMAP and SMTP; sends no mail.")
    print("The password is kept only in this process and is not saved by Postbird.")
    address = input("Gmail address (including Workspace domains): ").strip()
    if address.count("@") != 1 or any(char.isspace() for char in address):
        print("Enter a complete email address.", file=sys.stderr)
        return 2
    password = getpass.getpass("Google app password: ").replace(" ", "")
    if not password:
        print("No app password entered.", file=sys.stderr)
        return 2

    success = True
    try:
        count = probe_imap(address, password)
        print(f"IMAP: authenticated; Inbox opened read-only ({count} messages).")
    except (imaplib.IMAP4.error, OSError, ssl.SSLError, ValueError) as error:
        print(f"IMAP: failed ({type(error).__name__}).", file=sys.stderr)
        success = False

    try:
        probe_smtp(address, password)
        print("SMTP: authenticated; no message sent.")
    except (smtplib.SMTPException, OSError, ssl.SSLError) as error:
        print(f"SMTP: failed ({type(error).__name__}).", file=sys.stderr)
        success = False

    if not success:
        print(
            "Check the address, app password, account eligibility, and network access.",
            file=sys.stderr,
        )
    return 0 if success else 1


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (EOFError, KeyboardInterrupt):
        print("\nProbe cancelled; nothing was saved.", file=sys.stderr)
        sys.exit(130)
