import email
import email.policy
import unittest
from types import SimpleNamespace
from unittest.mock import MagicMock, patch

import gmail_imap_backend as backend


class GmailImapBackendTests(unittest.TestCase):
    def test_goa_lists_only_google_accounts_with_mail_and_xoauth2(self):
        def proxy(**values):
            result = MagicMock()
            result.get_cached_property.side_effect = lambda name: (
                SimpleNamespace(unpack=lambda: values[name]) if name in values else None
            )
            return result

        def obj(provider, email, smtp_auth=True):
            result = MagicMock()
            result.get_account.return_value = proxy(Id=email, ProviderType=provider)
            result.get_mail.return_value = proxy(
                EmailAddress=email,
                ImapSupported=True,
                SmtpSupported=True,
                SmtpAuthXoauth2=smtp_auth,
            )
            result.get_oauth2_based.return_value = MagicMock()
            return result

        client = MagicMock()
        client.get_accounts.return_value = [
            obj("google", "valid@example.com"),
            obj("google", "no-oauth-smtp@example.com", False),
            obj("microsoft", "other@example.com"),
        ]
        with patch("gmail_imap_backend.goa_client", return_value=(MagicMock(), client)):
            self.assertEqual(
                backend.goa_accounts(),
                [{"id": "valid@example.com", "email": "valid@example.com"}],
            )

    @patch("gmail_imap_backend.ssl.create_default_context")
    @patch("gmail_imap_backend.imaplib.IMAP4_SSL")
    def test_goa_imap_uses_xoauth2_instead_of_password_login(self, imap_ssl, _context):
        connection = imap_ssl.return_value
        request = {"email": "reader@example.com", "goa_id": "goa-123", "_goa_token": "short-lived"}
        backend.connect_imap(request)
        connection.login.assert_not_called()
        mechanism, response = connection.authenticate.call_args.args
        self.assertEqual(mechanism, "XOAUTH2")
        self.assertEqual(
            response(None), b"user=reader@example.com\x01auth=Bearer short-lived\x01\x01"
        )

    def test_goa_smtp_uses_xoauth2_instead_of_password_login(self):
        connection = MagicMock()
        connection.docmd.return_value = (235, b"OK")
        request = {"email": "reader@example.com", "goa_id": "goa-123", "_goa_token": "short-lived"}
        backend.authenticate_smtp(connection, request)
        connection.login.assert_not_called()
        self.assertEqual(connection.docmd.call_args.args[0], "AUTH XOAUTH2")

    def test_modified_utf7_mailbox_names_round_trip(self):
        label = "Projects & Café/日本語"
        self.assertEqual(backend.decode_mailbox(backend.encode_mailbox(label)), label)

    def test_unread_counts_use_status_without_selecting_or_fetching_mail(self):
        connection = MagicMock()
        connection.status.side_effect = [
            ("OK", [b'"INBOX" (MESSAGES 100 UNSEEN 12)']),
            ("OK", [b'"Projects" (UNSEEN 3)']),
        ]
        self.assertEqual(backend.unread_counts(connection, ["INBOX", "Projects"]), [12, 3])
        self.assertEqual(
            [call.args for call in connection.status.call_args_list],
            [('"INBOX"', "(UNSEEN)"), ('"Projects"', "(UNSEEN)")],
        )
        connection.select.assert_not_called()
        connection.fetch.assert_not_called()

    @patch("gmail_imap_backend.mailbox_name", return_value="[Gmail]/All Mail")
    def test_all_mail_unread_count_uses_its_mailbox(self, mailbox_name):
        connection = MagicMock()
        connection.status.return_value = ("OK", [b'"[Gmail]/All Mail" (UNSEEN 91)'])
        self.assertEqual(backend.unread_counts(connection, [""]), [91])
        mailbox_name.assert_called_once_with(connection, "ALL")

    @patch("gmail_imap_backend.mailbox_name", return_value="[Gmail]/All Mail")
    def test_marking_multiple_messages_read_uses_one_store(self, _mailbox_name):
        connection = MagicMock()
        connection.select.return_value = ("OK", [b"2"])

        def uid(command, *args):
            if command == "SEARCH":
                return "OK", [str({"one": 11, "two": 12}[args[-1]]).encode()]
            return "OK", [b""]

        connection.uid.side_effect = uid
        result = backend.set_unread_many(
            connection, {"ids": ["one", "two"], "value": False}
        )
        self.assertEqual(result, {"updated": ["one", "two"], "error": None})
        connection.select.assert_called_once_with("[Gmail]/All Mail", readonly=False)
        self.assertEqual(
            [call.args for call in connection.uid.call_args_list if call.args[0] == "STORE"],
            [("STORE", "11,12", "+FLAGS.SILENT", "(\\Seen)")],
        )

    def test_quotes_gmail_search_without_command_injection(self):
        self.assertEqual(backend.quoted('from:"Ada" \\ test'), '"from:\\"Ada\\" \\\\ test"')

    def test_parses_gmail_ids_flags_and_attachment_data(self):
        raw = (
            b"From: Sender <sender@example.com>\r\n"
            b"To: reader@example.com\r\n"
            b"Subject: Example\r\n"
            b"Content-Type: text/plain; charset=utf-8\r\n\r\nHello world"
        )
        meta = (
            b'1 (UID 7 X-GM-MSGID 123 X-GM-THRID 456 FLAGS (\\Seen \\Flagged) '
            b'X-GM-LABELS (\\Inbox) INTERNALDATE "17-Jul-2026 02:44:25 +0000" BODY[] {123}'
        )
        message = backend.message_from_row((meta, raw))
        self.assertEqual((message["id"], message["threadId"]), ("123", "456"))
        self.assertEqual(message["labelIds"], ["STARRED", "INBOX"])
        self.assertEqual(backend.decoded(message["payload"]["body"]["data"]), b"Hello world")

    def test_attachments_are_deferred_except_when_editing_a_draft(self):
        raw = (
            b"MIME-Version: 1.0\r\n"
            b"Content-Type: multipart/mixed; boundary=part\r\n\r\n"
            b"--part\r\nContent-Type: text/plain\r\n\r\nBody\r\n"
            b"--part\r\nContent-Type: application/octet-stream\r\n"
            b"Content-Disposition: attachment; filename=report.bin\r\n"
            b"Content-Transfer-Encoding: base64\r\n\r\nAQID\r\n--part--\r\n"
        )
        meta = b'1 (X-GM-MSGID 123 X-GM-THRID 456 FLAGS (\\Seen) INTERNALDATE "17-Jul-2026 02:44:25 +0000")'
        normal = backend.message_from_row((meta, raw))
        part = normal["payload"]["parts"][1]
        self.assertIsNone(part["body"]["data"])
        self.assertEqual(part["body"]["attachmentId"], "1")
        draft = backend.message_from_row((meta, raw), inline_attachments=True)
        self.assertEqual(backend.decoded(draft["payload"]["parts"][1]["body"]["data"]), b"\x01\x02\x03")

    def test_inline_image_without_filename_stays_downloadable(self):
        part = email.message_from_string(
            "Content-Type: image/png\nContent-Disposition: inline\n\nimage"
        )
        self.assertTrue(backend.is_attachment(part))
        self.assertEqual(backend.payload(part)["body"]["attachmentId"], "")

    @patch("gmail_imap_backend.locate", return_value=b"7")
    @patch("gmail_imap_backend.fetch_rows")
    def test_embedded_images_are_fetched_together_for_one_message(self, fetch_rows, locate):
        raw = (
            b"MIME-Version: 1.0\r\nContent-Type: multipart/related; boundary=part\r\n\r\n"
            b"--part\r\nContent-Type: text/html\r\n\r\n<img src=\"cid:logo\">\r\n"
            b"--part\r\nContent-Type: image/png\r\nContent-ID: <logo>\r\n"
            b"Content-Transfer-Encoding: base64\r\n\r\nAQID\r\n--part--\r\n"
        )
        fetch_rows.return_value = [(b"metadata", raw)]
        connection = MagicMock()
        self.assertEqual(
            backend.inline_images(connection, {"id": "123", "attachment_ids": ["1"]}),
            {"1": backend.encoded(b"\x01\x02\x03")},
        )
        locate.assert_called_once_with(connection, "123")
        fetch_rows.assert_called_once_with(connection, [b"7"], "(BODY.PEEK[])")

    def test_lists_threads_without_fetching_message_bodies(self):
        connection = MagicMock()
        connection.list.return_value = ("OK", [b'(\\HasNoChildren \\All) "/" "[Gmail]/All Mail"'])
        connection.select.return_value = ("OK", [b"2"])
        connection.fetch.return_value = (
            "OK", [b"1 (X-GM-THRID 123)", b"2 (X-GM-THRID 456)"],
        )
        result = backend.list_threads(connection, {"label": "ALL", "limit": 50})
        self.assertEqual(result, {"threads": [{"id": "456"}, {"id": "123"}]})
        connection.fetch.assert_called_once_with("1:2", "(X-GM-THRID)")
        connection.search.assert_not_called()
        connection.uid.assert_not_called()
        self.assertTrue(connection.select.call_args.kwargs["readonly"])

    def test_large_folder_fetches_only_recent_metadata(self):
        connection = MagicMock()
        connection.select.return_value = ("OK", [b"213545"])
        connection.fetch.return_value = (
            "OK", [f"{number} (X-GM-THRID {number})".encode() for number in range(213046, 213546)],
        )
        result = backend.list_threads(connection, {"label": "INBOX", "limit": 50})
        self.assertEqual(len(result["threads"]), 50)
        self.assertEqual(result["threads"][0]["id"], "213545")
        connection.fetch.assert_called_once_with("213046:213545", "(X-GM-THRID)")
        connection.uid.assert_not_called()

    def test_fetches_older_batch_when_recent_messages_share_one_thread(self):
        connection = MagicMock()
        connection.select.return_value = ("OK", [b"1000"])
        connection.fetch.side_effect = [
            ("OK", [f"{number} (X-GM-THRID 7)".encode() for number in range(501, 1001)]),
            ("OK", [b"500 (X-GM-THRID 8)"]),
        ]
        result = backend.list_threads(connection, {"label": "INBOX", "limit": 2})
        self.assertEqual(result, {"threads": [{"id": "7"}, {"id": "8"}]})
        self.assertEqual(
            [call.args[0] for call in connection.fetch.call_args_list],
            ["501:1000", "1:500"],
        )

    def test_search_is_bounded_and_walks_back_to_older_matches(self):
        connection = MagicMock()
        connection.select.return_value = ("OK", [b"1001"])
        connection.search.side_effect = [("OK", [b"1000"]), ("OK", [b"2"])]
        connection.fetch.side_effect = [
            ("OK", [b"1000 (X-GM-THRID 7)"]),
            ("OK", [b"2 (X-GM-THRID 8)"]),
        ]
        result = backend.list_threads(connection, {"label": "INBOX", "query": 'from:"Ada"', "limit": 2})
        self.assertEqual(result, {"threads": [{"id": "7"}, {"id": "8"}]})
        self.assertEqual(
            [call.args for call in connection.search.call_args_list],
            [
                (None, "502:1001", "X-GM-RAW", '"from:\\"Ada\\""'),
                (None, "2:501", "X-GM-RAW", '"from:\\"Ada\\""'),
            ],
        )
        self.assertEqual([call.args[0] for call in connection.fetch.call_args_list], ["1000", "2"])
        connection.uid.assert_not_called()

    def test_unread_filter_finds_old_messages_with_same_flag_as_badge(self):
        connection = MagicMock()
        connection.select.return_value = ("OK", [b"100000"])
        connection.search.return_value = ("OK", [b"4 10 25"])
        connection.fetch.return_value = (
            "OK", [b"4 (X-GM-THRID 7)", b"10 (X-GM-THRID 8)", b"25 (X-GM-THRID 9)"],
        )
        result = backend.list_threads(connection, {"label": "INBOX", "query": "is:unread"})
        self.assertEqual(result, {"threads": [{"id": "9"}, {"id": "8"}, {"id": "7"}]})
        connection.search.assert_called_once_with(None, "UNSEEN")
        connection.fetch.assert_called_once_with("4,10,25", "(X-GM-THRID)")

    @patch("gmail_imap_backend.connect_imap")
    @patch("gmail_imap_backend.mailbox_name", side_effect=lambda _connection, label: label)
    @patch("gmail_imap_backend.ids")
    @patch("gmail_imap_backend.fetch_messages")
    def test_batch_fetch_reuses_one_imap_connection_and_selects_each_mailbox_once(
        self, fetch_messages, search_ids, _mailbox_name, connect_imap
    ):
        connect_imap.return_value.select.return_value = ("OK", [b"1"])
        search_ids.side_effect = lambda _connection, _criterion, thread_id: {
            "one": [b"1"], "two": [b"2"]
        }[thread_id]
        fetch_messages.side_effect = lambda _connection, _uids: [
            {"id": "a", "threadId": "one", "internalDate": "1"},
            {"id": "b", "threadId": "two", "internalDate": "2"},
        ]
        result = backend.dispatch({"operation": "threads", "ids": ["one", "two"]})
        self.assertEqual(
            [[message["id"] for message in item["messages"]] for item in result],
            [["a"], ["b"]],
        )
        connect_imap.assert_called_once()
        self.assertEqual(connect_imap.return_value.select.call_count, 3)
        self.assertEqual(fetch_messages.call_count, 3)
        self.assertEqual(
            [call.args[1] for call in fetch_messages.call_args_list],
            [[b"1", b"2"]] * 3,
        )
        connect_imap.return_value.logout.assert_called_once()

    @patch("gmail_imap_backend.mailbox_name", return_value="[Gmail]/All Mail")
    def test_archive_removes_inbox_label_only(self, _mailbox_name):
        connection = MagicMock()
        connection.select.return_value = ("OK", [b"2"])
        connection.uid.side_effect = [("OK", [b"12 13"]), ("OK", [b""])]
        with patch("gmail_imap_backend.connect_imap", return_value=connection):
            backend.dispatch({"operation": "archive_thread", "id": "123"})
        self.assertEqual(
            connection.uid.call_args_list[1].args,
            ("STORE", "12,13", "-X-GM-LABELS", "(\\Inbox)"),
        )

    @patch("gmail_imap_backend.mailbox_name")
    def test_draft_replacement_moves_only_expected_old_draft(self, mailbox_name):
        mailbox_name.side_effect = lambda _connection, label: f"[Gmail]/{label}"
        connection = MagicMock()
        connection.select.return_value = ("OK", [b"1"])
        connection.uid.side_effect = [("OK", [b"77"]), ("OK", [b""])]
        connection.append.return_value = ("OK", [b""])
        request = {
            "id": "123",
            "expected_message_id": "123",
            "send": False,
            "raw": backend.encoded(b"Subject: Draft\r\n\r\nBody"),
        }
        backend.existing_draft(connection, request)
        self.assertEqual(
            connection.uid.call_args_list[-1].args,
            ("MOVE", b"77", '"[Gmail]/TRASH"'),
        )

    @patch("gmail_imap_backend.mailbox_name")
    def test_changed_draft_is_never_sent_or_replaced(self, mailbox_name):
        mailbox_name.return_value = "[Gmail]/Drafts"
        connection = MagicMock()
        connection.select.return_value = ("OK", [b"1"])
        connection.uid.return_value = ("OK", [b""])
        with patch("gmail_imap_backend.send") as send:
            with self.assertRaisesRegex(RuntimeError, "changed elsewhere"):
                backend.existing_draft(
                    connection,
                    {"id": "123", "expected_message_id": "123", "send": True},
                )
            send.assert_not_called()
            connection.append.assert_not_called()

    @patch("gmail_imap_backend.ssl.create_default_context")
    @patch("gmail_imap_backend.smtplib.SMTP_SSL")
    def test_send_strips_bcc_header_but_uses_bcc_envelope(self, smtp_ssl, _context):
        connection = smtp_ssl.return_value.__enter__.return_value
        raw = (
            b"From: sender@example.com\r\n"
            b"To: primary@example.com\r\n"
            b"Bcc: hidden@example.com\r\n"
            b"Subject: Example\r\n\r\nBody"
        )
        backend.send({"email": "sender@example.com", "password": "secret", "raw": backend.encoded(raw)})
        sender, recipients, transmitted = connection.sendmail.call_args.args
        self.assertEqual(sender, "sender@example.com")
        self.assertEqual(recipients, ["primary@example.com", "hidden@example.com"])
        self.assertNotIn("Bcc", email.message_from_bytes(transmitted, policy=email.policy.default))


if __name__ == "__main__":
    unittest.main()
