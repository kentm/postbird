import email
import email.policy
import imaplib
import io
import json
import socket
import threading
import unittest
from types import SimpleNamespace
from unittest.mock import MagicMock, patch

import gmail_imap_backend as backend


class GmailImapBackendTests(unittest.TestCase):
    @patch("gmail_imap_backend.list_threads")
    @patch("gmail_imap_backend.connect_imap")
    def test_recent_and_unread_folder_lists_share_one_connection(self, connect, list_threads):
        list_threads.side_effect = [{"threads": [{"id": "recent"}]}, {"threads": [{"id": "old-unread"}]}]
        result = backend.dispatch({"operation": "folder_references", "label": "INBOX", "include_unread": True})
        self.assertEqual(result, [[{"id": "recent"}], [{"id": "old-unread"}]])
        connect.assert_called_once()
        self.assertEqual(list_threads.call_args_list[0].args, (connect.return_value, {"label": "INBOX", "limit": 50}))
        self.assertEqual(list_threads.call_args_list[1].args, (connect.return_value, {"label": "INBOX", "limit": 50, "query": "is:unread"}))
        connect.return_value.logout.assert_called_once()

    @patch("gmail_imap_backend.list_threads", return_value={"threads": []})
    @patch("gmail_imap_backend.connect_imap")
    def test_folder_without_unread_prefetch_omits_the_second_query(self, connect, list_threads):
        self.assertEqual(backend.dispatch({"operation": "folder_references", "label": "TRASH", "include_unread": False}), [[], []])
        list_threads.assert_called_once_with(connect.return_value, {"label": "TRASH", "limit": 50})

    def test_idle_renews_without_fetching_or_repeating_ready(self):
        connection = MagicMock()
        connection.capabilities = ("IMAP4rev1", "IDLE")
        connection.select.return_value = ("OK", [b"1"])
        connection.response.return_value = (None, [None])
        quiet, changes = MagicMock(), MagicMock()
        quiet.__enter__.return_value = iter([])
        changes.__enter__.return_value = iter([
            ("OK", [b"still here"]), ("EXISTS", [b"2"]),
            ("FETCH", [b"1 (FLAGS (\\Seen))"]), ("EXPUNGE", [b"1"]),
        ])
        connection.idle.side_effect = [quiet, changes, imaplib.IMAP4.abort("expired")]
        output = io.StringIO()
        with patch("sys.stdout", output), self.assertRaises(imaplib.IMAP4.abort):
            backend.watch_inbox(connection)
        events = [json.loads(line)["result"]["event"] for line in output.getvalue().splitlines()]
        self.assertEqual(events, ["ready", "changed", "changed", "changed"])
        self.assertEqual(connection.idle.call_count, 3)
        connection.idle.assert_called_with(duration=1200)
        connection.select.assert_called_once_with("INBOX", readonly=True)
        connection.fetch.assert_not_called()
        connection.status.assert_not_called()
        connection.uid.assert_not_called()

    def test_idle_preserves_changes_received_while_renewing(self):
        connection = MagicMock()
        connection.capabilities = ("IDLE",)
        connection.select.return_value = ("OK", [b"1"])
        quiet = MagicMock()
        quiet.__enter__.return_value = iter([])
        connection.idle.side_effect = [quiet, imaplib.IMAP4.abort("expired")]
        connection.response.side_effect = [
            ("EXISTS", [b"1"]), ("EXPUNGE", [None]), ("FETCH", [None]),
            ("EXISTS", [b"2"]), ("EXPUNGE", [None]), ("FETCH", [b"1 (FLAGS (\\Seen))"]),
        ]
        output = io.StringIO()
        with patch("sys.stdout", output), self.assertRaises(imaplib.IMAP4.abort):
            backend.watch_inbox(connection)
        self.assertEqual(
            [json.loads(line)["result"]["event"] for line in output.getvalue().splitlines()],
            ["ready", "changed"],
        )
        self.assertEqual(connection.response.call_count, 6)

    def test_idle_unavailable_has_a_clear_fallback(self):
        for connection in (
            SimpleNamespace(capabilities=("IDLE",)),
            SimpleNamespace(capabilities=("IMAP4rev1",), idle=lambda **_kwargs: None),
        ):
            with self.subTest(connection=connection), self.assertRaisesRegex(RuntimeError, "periodic checks"):
                backend.watch_inbox(connection)

    @patch("gmail_imap_backend.watch_inbox", side_effect=imaplib.IMAP4.abort("expired secret-token"))
    @patch("gmail_imap_backend.connect_imap")
    @patch("gmail_imap_backend.goa_token", side_effect=["first-token", "fresh-token"])
    def test_idle_reconnect_uses_fresh_goa_token_and_closes_old_socket(self, token, connect, watch):
        request = {"operation": "watch_inbox", "email": "test@example.com", "goa_id": "1"}
        for expected in ("first-token", "fresh-token"):
            with self.assertRaises(imaplib.IMAP4.abort):
                backend.dispatch(request)
            self.assertEqual(request["_goa_token"], expected)
        self.assertEqual(token.call_count, 2)
        self.assertEqual(connect.return_value.shutdown.call_count, 2)
        connect.return_value.logout.assert_not_called()
        self.assertEqual(watch.call_count, 2)

    @patch("gmail_imap_backend.watch_inbox", side_effect=imaplib.IMAP4.abort("[OVERQUOTA] secret-token"))
    @patch("gmail_imap_backend.connect_imap")
    @patch("gmail_imap_backend.goa_token")
    def test_idle_app_password_retains_quota_protection(self, token, connect, _watch):
        output = io.StringIO()
        request = {"operation": "watch_inbox", "email": "test@example.com", "password": "secret-token"}
        with patch("sys.stdin", io.StringIO(json.dumps(request))), patch("sys.stdout", output):
            backend.main()
        token.assert_not_called()
        connect.assert_called_once_with(request)
        self.assertEqual(json.loads(output.getvalue())["error_kind"], "rate_limited")
        self.assertNotIn("secret-token", output.getvalue())

    @unittest.skipUnless(hasattr(imaplib.IMAP4, "idle"), "requires Python 3.14")
    def test_idle_streams_real_protocol_events_including_before_acknowledgement(self):
        listener = socket.socket()
        listener.bind(("127.0.0.1", 0))
        listener.listen(1)
        listener.settimeout(5)
        commands, failures = [], []

        def server():
            try:
                with listener.accept()[0] as peer:
                    peer.settimeout(5)
                    with peer.makefile("rwb", buffering=0) as stream:
                        stream.write(b"* OK test server\r\n")
                        while line := stream.readline():
                            tag, command, *_args = line.split()
                            commands.append(command)
                            if command == b"CAPABILITY":
                                stream.write(b"* CAPABILITY IMAP4rev1 IDLE\r\n" + tag + b" OK completed\r\n")
                            elif command == b"LOGIN":
                                stream.write(tag + b" OK completed\r\n")
                            elif command == b"EXAMINE":
                                stream.write(b"* 1 EXISTS\r\n" + tag + b" OK [READ-ONLY] completed\r\n")
                            elif command == b"IDLE":
                                stream.write(b"* 2 EXISTS\r\n+ idling\r\n* 1 FETCH (FLAGS (\\Seen))\r\n")
                                # EOF simulates a network drop after the two hints.
                                return
                            else:
                                raise AssertionError(line)
            except Exception as error:
                failures.append(error)

        worker = threading.Thread(target=server)
        worker.start()
        output = io.StringIO()
        try:
            connection = imaplib.IMAP4("127.0.0.1", listener.getsockname()[1], timeout=5)
            connection.login("user", "password")
            try:
                with patch("sys.stdout", output), self.assertRaises((imaplib.IMAP4.abort, OSError)):
                    backend.watch_inbox(connection)
            finally:
                connection.shutdown()
        finally:
            worker.join(timeout=6)
            listener.close()
        self.assertFalse(worker.is_alive())
        self.assertEqual(failures, [])
        self.assertEqual(commands, [b"CAPABILITY", b"LOGIN", b"CAPABILITY", b"EXAMINE", b"IDLE"])
        self.assertEqual(
            [json.loads(line)["result"]["event"] for line in output.getvalue().splitlines()],
            ["ready", "changed", "changed"],
        )

    @patch("gmail_imap_backend.dispatch_once")
    def test_gmail_overquota_is_recognized_without_reconnecting(self, dispatch_once):
        dispatch_once.side_effect = imaplib.IMAP4.abort(
            "command: EXAMINE => [OVERQUOTA] Account exceeded command or bandwidth limits. secret-token"
        )
        output = io.StringIO()
        with patch("sys.stdin", io.StringIO('{"operation": "list_threads"}')), patch("sys.stdout", output):
            backend.main()
        dispatch_once.assert_called_once()
        result = json.loads(output.getvalue())
        self.assertFalse(result["ok"])
        self.assertEqual(result["error_kind"], "rate_limited")
        self.assertIn("OVERQUOTA", result["error"])
        self.assertNotIn("secret-token", output.getvalue())

    def test_overquota_no_reply_has_the_same_classification(self):
        with self.assertRaises(backend.MailQuotaError):
            backend.require_ok(("NO", [b"[OVERQUOTA] Too much bandwidth"]), "FETCH")

    @patch("gmail_imap_backend.mailbox_name", side_effect=lambda _connection, label: label)
    def test_read_status_update_preserves_partial_progress_when_limited(self, _mailbox_name):
        connection = MagicMock()
        connection.select.side_effect = [
            ("OK", [b"1"]),
            imaplib.IMAP4.abort("[OVERQUOTA] Account exceeded command or bandwidth limits."),
        ]
        connection.uid.side_effect = [("OK", [b"7"]), ("OK", [b""]), ("OK", [b""])]
        result = backend.set_unread_many(connection, {"ids": ["first", "second"], "value": False})
        self.assertEqual(result["updated"], ["first"])
        self.assertEqual(result["error_kind"], "rate_limited")
        self.assertIn("OVERQUOTA", result["error"])

    @patch("gmail_imap_backend.connect_imap")
    def test_refresh_reconnects_and_restarts_after_an_imap_abort(self, connect_imap):
        interrupted, recovered = MagicMock(), MagicMock()
        connect_imap.side_effect = [interrupted, recovered]
        interrupted.select.return_value = ("OK", [b"2"])
        interrupted.fetch.side_effect = imaplib.IMAP4.abort("socket error: EOF")
        # A deletion happened between attempts; the new session must reselect
        # the mailbox and use its current message count.
        recovered.select.return_value = ("OK", [b"1"])
        recovered.fetch.return_value = ("OK", [b"1 (X-GM-THRID 456)"])
        result = backend.dispatch({"operation": "list_threads", "label": "INBOX"})
        self.assertEqual(result, {"threads": [{"id": "456"}]})
        self.assertEqual(connect_imap.call_count, 2)
        interrupted.fetch.assert_called_once_with("1:2", "(X-GM-THRID)")
        interrupted.shutdown.assert_called_once()
        interrupted.logout.assert_not_called()
        recovered.select.assert_called_once_with("INBOX", readonly=True)
        recovered.fetch.assert_called_once_with("1:1", "(X-GM-THRID)")
        recovered.logout.assert_called_once()

    @patch("gmail_imap_backend.dispatch_once")
    def test_read_reconnect_is_bounded_and_reports_no_server_secrets(self, dispatch_once):
        dispatch_once.side_effect = imaplib.IMAP4.abort("server echoed secret-token")
        output = io.StringIO()
        with patch("sys.stdin", io.StringIO('{"operation": "threads"}')), patch("sys.stdout", output):
            backend.main()
        self.assertEqual(dispatch_once.call_count, 2)
        result = json.loads(output.getvalue())
        self.assertFalse(result["ok"])
        self.assertIn("loading conversations", result["error"])
        self.assertIn("reconnect", result["error"])
        self.assertNotIn("authentication", result["error"])
        self.assertNotIn("secret-token", output.getvalue())

    @patch("gmail_imap_backend.dispatch_once")
    def test_authentication_errors_are_not_retried(self, dispatch_once):
        dispatch_once.side_effect = imaplib.IMAP4.error("authentication failed")
        with self.assertRaises(imaplib.IMAP4.error):
            backend.dispatch({"operation": "list_threads"})
        dispatch_once.assert_called_once()

    @patch("gmail_imap_backend.dispatch_once")
    def test_writes_are_never_replayed_after_an_abort(self, dispatch_once):
        dispatch_once.side_effect = imaplib.IMAP4.abort("lost acknowledgement")
        for operation in ("send", "create_draft", "write_existing_draft", "delete_draft", "trash",
                          "archive_thread", "set_unread", "set_unread_many", "set_starred"):
            with self.subTest(operation=operation):
                dispatch_once.reset_mock()
                with self.assertRaises(imaplib.IMAP4.abort):
                    backend.dispatch({"operation": operation})
                dispatch_once.assert_called_once()

    @patch("gmail_imap_backend.mailbox_name", side_effect=lambda _connection, label: label)
    def test_thread_refresh_keeps_surviving_messages_when_a_searched_uid_disappears(self, _mailbox_name):
        connection = MagicMock()
        connection.select.return_value = ("OK", [b"2"])
        connection.uid.side_effect = [
            ("OK", [b"7 8"]),
            # UID 7 was deleted after SEARCH; FETCH returns only the survivor.
            ("OK", [
                (b'1 (UID 8 X-GM-MSGID 108 X-GM-THRID 456 FLAGS (\\Seen) INTERNALDATE "23-Sep-2026 09:00:00 +0000" BODY[] {40}',
                 b"Subject: Survivor\r\nContent-Type: text/plain\r\n\r\nRemaining body"),
                b")",
            ]),
            ("OK", [b""]),  # Drafts
            ("OK", [b""]),  # Trash (permanently deleted message)
        ]
        result = backend.threads(connection, ["456"])
        self.assertEqual([message["id"] for message in result[0]["messages"]], ["108"])
        self.assertEqual(result[0]["messages"][0]["snippet"], "Remaining body")

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

    def test_new_mail_poll_baselines_without_alerting_for_existing_mail(self):
        connection = MagicMock()
        connection.status.return_value = ("OK", [b'"INBOX" (UIDVALIDITY 9 UIDNEXT 12)'])
        self.assertEqual(
            backend.poll_new_mail(connection, {}),
            {"cursor": {"uidvalidity": 9, "uidnext": 12}, "messages": []},
        )
        connection.select.assert_not_called()
        connection.uid.assert_not_called()

    def test_new_mail_poll_fetches_only_new_unread_headers(self):
        connection = MagicMock()
        connection.status.return_value = ("OK", [b'"INBOX" (UIDVALIDITY 9 UIDNEXT 12)'])
        connection.select.return_value = ("OK", [b"12"])
        connection.uid.side_effect = [
            ("OK", [b"10 11"]),
            ("OK", [
                (b"1 (UID 10 X-GM-MSGID 101 X-GM-THRID 201 FLAGS ())",
                 b"From: Ada <ada@example.com>\r\nSubject: Hello\r\n\r\n"),
                (b"2 (UID 11 X-GM-MSGID 102 X-GM-THRID 202 FLAGS (\\Seen))",
                 b"From: Read <read@example.com>\r\nSubject: Already read\r\n\r\n"),
            ]),
        ]
        result = backend.poll_new_mail(connection, {"cursor": {"uidvalidity": 9, "uidnext": 10}})
        self.assertEqual(result, {
            "cursor": {"uidvalidity": 9, "uidnext": 12},
            "messages": [{"id": "101", "thread_id": "201", "sender": "Ada <ada@example.com>", "subject": "Hello"}],
        })
        self.assertEqual(connection.uid.call_args_list[0].args,
                         ("SEARCH", None, "UNSEEN", "UID", "10:11"))
        self.assertIn("BODY.PEEK[HEADER.FIELDS (FROM SUBJECT)]",
                      connection.uid.call_args_list[1].args[2])

    def test_new_mail_poll_rebaselines_when_uidvalidity_changes(self):
        connection = MagicMock()
        connection.status.return_value = ("OK", [b'"INBOX" (UIDVALIDITY 10 UIDNEXT 500)'])
        result = backend.poll_new_mail(connection, {"cursor": {"uidvalidity": 9, "uidnext": 10}})
        self.assertEqual(result["messages"], [])
        self.assertEqual(result["cursor"], {"uidvalidity": 10, "uidnext": 500})
        connection.uid.assert_not_called()

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

    @patch("gmail_imap_backend.mailbox_name", side_effect=lambda _connection, label: label)
    def test_phone_draft_without_flags_or_labels_is_identified_by_its_mailbox(self, _mailbox_name):
        connection = MagicMock()
        connection.select.return_value = ("OK", [b"1"])
        selected = []
        connection.select.side_effect = lambda name, **_kwargs: (selected.append(name) or ("OK", [b"1"]))
        # The same draft appears in All Mail and Drafts, alongside a sent
        # message in its thread. Match the metadata observed from Gmail.
        raw = b"From: Kent <kent@otron.com>\r\nTo: reader@example.com\r\n\r\nPhone draft"
        draft_meta = b"1 (X-GM-THRID 456 X-GM-MSGID 123 X-GM-LABELS () UID 7 FLAGS (\\Seen))"
        sent_meta = b"2 (X-GM-THRID 456 X-GM-MSGID 124 X-GM-LABELS (\\Sent) UID 8 FLAGS (\\Seen))"
        def uid(command, *args):
            if command == "SEARCH":
                return "OK", [{"ALL": b"7 8", "DRAFT": b"7", "TRASH": b""}[selected[-1]]]
            self.assertEqual(command, "FETCH")
            rows = [(draft_meta, raw)]
            if selected[-1] == "ALL":
                rows.append((sent_meta, b"From: reader@example.com\r\n\r\nSent message"))
            return "OK", rows
        connection.uid.side_effect = uid
        messages = backend.threads(connection, ["456"])[0]["messages"]
        by_id = {message["id"]: message for message in messages}
        self.assertEqual(by_id["123"]["labelIds"], ["DRAFT"])
        self.assertEqual(by_id["124"]["labelIds"], ["SENT"])
        self.assertEqual(len(messages), 2)
        with patch("gmail_imap_backend.connect_imap", return_value=connection):
            draft = backend.dispatch({"operation": "draft_for_message", "id": "123"})
        self.assertEqual(draft["message"]["labelIds"], ["DRAFT"])
        self.assertEqual(backend.decoded(draft["message"]["payload"]["body"]["data"]), b"Phone draft")

    @patch("gmail_imap_backend.fetch_rows")
    def test_draft_flag_and_mailbox_do_not_duplicate_the_label(self, fetch_rows):
        fetch_rows.return_value = [(b"1 (X-GM-MSGID 123 X-GM-THRID 456 FLAGS (\\Seen \\Draft) X-GM-LABELS (\\Drafts))", b"\r\nDraft")]
        message = backend.fetch_messages(MagicMock(), [b"7"], mailbox_label="DRAFT")[0]
        self.assertEqual(message["labelIds"], ["DRAFT"])

    def test_inline_image_without_filename_stays_downloadable(self):
        part = email.message_from_string(
            "Content-Type: image/png\nContent-Disposition: inline\n\nimage"
        )
        self.assertTrue(backend.is_attachment(part))
        self.assertEqual(backend.payload(part)["body"]["attachmentId"], "")

    def test_new_mail_retains_embedded_images_with_the_cached_body(self):
        raw = (
            b"MIME-Version: 1.0\r\nContent-Type: multipart/related; boundary=part\r\nContent-ID: <container>\r\n\r\n"
            b"--part\r\nContent-Type: text/html\r\nContent-ID: <html-body>\r\n\r\n<p>Hello</p><img src=\"cid:logo\">\r\n"
            b"--part\r\nContent-Type: image/png\r\nContent-ID: <logo>\r\n"
            b"Content-Disposition: inline; filename=logo.png\r\n"
            b"Content-Transfer-Encoding: base64\r\n\r\nAQID\r\n--part--\r\n"
        )
        meta = b'1 (X-GM-MSGID 123 X-GM-THRID 456 FLAGS () INTERNALDATE "17-Jul-2026 02:44:25 +0000")'
        message = backend.message_from_row((meta, raw))
        html, image = message["payload"]["parts"]
        self.assertIn(b"<p>Hello</p>", backend.decoded(html["body"]["data"]))
        self.assertEqual(backend.decoded(image["body"]["data"]), b"\x01\x02\x03")
        self.assertIsNone(image["body"]["attachmentId"])

    def test_marketing_html_with_content_id_is_cached_as_body(self):
        raw = (
            b"MIME-Version: 1.0\r\nContent-Type: multipart/alternative; boundary=part\r\n\r\n"
            b"--part\r\nContent-Type: text/plain\r\n\r\nLong tracking URLs\r\n"
            b"--part\r\nContent-Type: text/html; charset=iso-8859-1\r\n"
            b"Content-Id: <html-body@example.com>\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n"
            b"<html><body><h1>Caf=E9 sale</h1></body></html>\r\n--part--\r\n"
        )
        message = backend.message_from_row((b"1 (X-GM-MSGID 123 X-GM-THRID 456 FLAGS (\\Seen))", raw))
        html = message["payload"]["parts"][1]
        self.assertEqual(backend.decoded(html["body"]["data"]).decode(), "<html><body><h1>Café sale</h1></body></html>")
        self.assertIsNone(html["body"]["attachmentId"])
        for mime_type in ("text/plain", "text/html"):
            for disposition in ("attachment", 'inline; filename="saved.html"'):
                with self.subTest(mime_type=mime_type, disposition=disposition):
                    part = email.message_from_string(f"Content-Type: {mime_type}\nContent-ID: <file>\nContent-Disposition: {disposition}\n\nFile")
                    self.assertTrue(backend.is_attachment(part))
                    self.assertIsNone(backend.payload(part)["body"]["data"])

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
        fetch_messages.side_effect = lambda _connection, _uids, **_kwargs: [
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

    @patch("gmail_imap_backend.mailbox_name", side_effect=lambda _connection, label: f"[Gmail]/{label}")
    def test_delete_draft_moves_only_the_requested_draft_to_trash(self, _mailbox_name):
        connection = MagicMock()
        connection.select.return_value = ("OK", [b"3"])
        connection.uid.side_effect = [("OK", [b"77"]), ("OK", [b""])]
        with patch("gmail_imap_backend.connect_imap", return_value=connection):
            backend.dispatch({"operation": "delete_draft", "id": "123"})
        connection.select.assert_called_once_with("[Gmail]/DRAFT", readonly=False)
        self.assertEqual(connection.uid.call_args_list[0].args, ("SEARCH", None, "X-GM-MSGID", "123"))
        self.assertEqual(connection.uid.call_args_list[1].args, ("MOVE", b"77", '\"[Gmail]/TRASH\"'))
        self.assertEqual(connection.uid.call_count, 2)
        connection.expunge.assert_not_called()
        connection.logout.assert_called_once()

    @patch("gmail_imap_backend.mailbox_name", return_value="[Gmail]/Drafts")
    def test_delete_draft_rejects_missing_or_ambiguous_drafts(self, _mailbox_name):
        for found in (b"", b"77 78"):
            connection = MagicMock()
            connection.select.return_value = ("OK", [b"3"])
            connection.uid.return_value = ("OK", [found])
            with self.assertRaisesRegex(RuntimeError, "changed elsewhere"):
                backend.delete_draft(connection, {"id": "123"})
            connection.uid.assert_called_once_with("SEARCH", None, "X-GM-MSGID", "123")
            connection.expunge.assert_not_called()

    @patch("gmail_imap_backend.mailbox_name", side_effect=lambda _connection, label: label)
    def test_delete_draft_reports_move_failure(self, _mailbox_name):
        connection = MagicMock()
        connection.select.return_value = ("OK", [b"1"])
        connection.uid.side_effect = [("OK", [b"77"]), ("NO", [b"Could not move"])]
        with self.assertRaisesRegex(RuntimeError, "MOVE draft to Trash"):
            backend.delete_draft(connection, {"id": "123"})
        self.assertEqual(connection.uid.call_count, 2)

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
    def test_alias_sender_uses_primary_account_credentials(self, smtp_ssl, _context):
        connection = smtp_ssl.return_value.__enter__.return_value
        raw = b"From: kent@otron.com\r\nTo: reader@example.com\r\n\r\nHello"
        request = {
            "email": "kent@otron.net", "sender": "kent@otron.com",
            "password": "secret", "raw": backend.encoded(raw),
        }
        backend.send(request)
        connection.login.assert_called_once_with("kent@otron.net", "secret")
        sender, recipients, transmitted = connection.sendmail.call_args.args
        self.assertEqual(sender, "kent@otron.com")
        self.assertEqual(recipients, ["reader@example.com"])
        self.assertEqual(email.message_from_bytes(transmitted)["From"], "kent@otron.com")
        connection.reset_mock()
        connection.docmd.return_value = (235, b"OK")
        request.update(goa_id="goa-123", _goa_token="short-lived")
        backend.send(request)
        connection.login.assert_not_called()
        import base64
        auth = base64.b64decode(connection.docmd.call_args.args[1]).decode()
        self.assertIn("user=kent@otron.net\x01", auth)
        self.assertEqual(connection.sendmail.call_args.args[0], "kent@otron.com")

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
