import unittest
from unittest.mock import MagicMock, patch

import gmail_app_password_probe as probe


class GmailAppPasswordProbeTests(unittest.TestCase):
    @patch("gmail_app_password_probe.ssl.create_default_context")
    @patch("gmail_app_password_probe.imaplib.IMAP4_SSL")
    def test_imap_opens_inbox_read_only(self, imap_ssl, ssl_context):
        connection = MagicMock()
        imap_ssl.return_value.__enter__.return_value = connection
        connection.select.return_value = ("OK", [b"42"])

        self.assertEqual(probe.probe_imap("person@example.com", "secret"), 42)
        imap_ssl.assert_called_once_with(
            "imap.gmail.com", 993, ssl_context=ssl_context.return_value, timeout=15
        )
        connection.login.assert_called_once_with("person@example.com", "secret")
        connection.select.assert_called_once_with("INBOX", readonly=True)

    @patch("gmail_app_password_probe.ssl.create_default_context")
    @patch("gmail_app_password_probe.smtplib.SMTP_SSL")
    def test_smtp_authenticates_without_sending(self, smtp_ssl, ssl_context):
        connection = MagicMock()
        smtp_ssl.return_value.__enter__.return_value = connection

        probe.probe_smtp("person@example.com", "secret")

        smtp_ssl.assert_called_once_with(
            "smtp.gmail.com", 465, context=ssl_context.return_value, timeout=15
        )
        connection.ehlo.assert_called_once_with()
        connection.login.assert_called_once_with("person@example.com", "secret")
        connection.sendmail.assert_not_called()
        connection.send_message.assert_not_called()


if __name__ == "__main__":
    unittest.main()
