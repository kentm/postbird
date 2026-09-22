"""Postbird's Gmail IMAP/SMTP transport. JSON requests and replies use stdin/stdout.

App passwords are supplied only on stdin. GOA tokens stay inside this process.
Never include either credential in errors or logs.
"""

import base64
import email
import email.policy
import email.utils
import imaplib
import json
import re
import smtplib
import ssl
import sys
import time


TIMEOUT = 30
SPECIAL = {
    "INBOX": "INBOX",
    "STARRED": "[Gmail]/Starred",
    "SENT": "[Gmail]/Sent Mail",
    "DRAFT": "[Gmail]/Drafts",
    "TRASH": "[Gmail]/Trash",
    "ALL": "[Gmail]/All Mail",
}


def encoded(data):
    return base64.urlsafe_b64encode(data).decode("ascii").rstrip("=")


def decoded(value):
    return base64.urlsafe_b64decode(value + "=" * (-len(value) % 4))


def quoted(value):
    return '"' + value.replace("\\", "\\\\").replace('"', '\\"') + '"'


def encode_mailbox(value):
    pieces = []
    pending = ""
    for char in value:
        if 0x20 <= ord(char) <= 0x7E:
            if pending:
                pieces.append("&" + base64.b64encode(pending.encode("utf-16-be")).decode().rstrip("=").replace("/", ",") + "-")
                pending = ""
            pieces.append("&-" if char == "&" else char)
        else:
            pending += char
    if pending:
        pieces.append("&" + base64.b64encode(pending.encode("utf-16-be")).decode().rstrip("=").replace("/", ",") + "-")
    return "".join(pieces)


def decode_mailbox(value):
    def replace(match):
        encoded_chunk = match.group(1)
        if not encoded_chunk:
            return "&"
        data = base64.b64decode(encoded_chunk.replace(",", "/") + "=" * (-len(encoded_chunk) % 4))
        return data.decode("utf-16-be")

    return re.sub(r"&([A-Za-z0-9+,]*)-", replace, value)


def require_ok(reply, operation):
    status, data = reply
    if status != "OK":
        raise RuntimeError(f"IMAP {operation} failed")
    return data


def goa_client():
    try:
        import gi

        gi.require_version("Goa", "1.0")
        from gi.repository import Goa

        return Goa, Goa.Client.new_sync(None)
    except (ImportError, ValueError) as error:
        raise RuntimeError("GNOME Online Accounts Python bindings are unavailable") from error
    except Exception as error:
        raise RuntimeError("Could not connect to GNOME Online Accounts") from error


def goa_property(proxy, name):
    value = proxy.get_cached_property(name)
    return value.unpack() if value is not None else None


def eligible_goa_account(obj):
    account, mail, oauth = obj.get_account(), obj.get_mail(), obj.get_oauth2_based()
    return (
        account is not None
        and mail is not None
        and oauth is not None
        and bool(goa_property(account, "Id"))
        and goa_property(account, "ProviderType") == "google"
        and goa_property(account, "MailDisabled") is not True
        and goa_property(mail, "ImapSupported") is True
        and goa_property(mail, "SmtpSupported") is True
        and goa_property(mail, "SmtpAuthXoauth2") is True
        and bool(goa_property(mail, "EmailAddress"))
    )


def goa_accounts():
    _goa, client = goa_client()
    return [
        {
            "id": goa_property(obj.get_account(), "Id"),
            "email": goa_property(obj.get_mail(), "EmailAddress"),
        }
        for obj in client.get_accounts()
        if eligible_goa_account(obj)
    ]


def goa_token(request):
    goa, client = goa_client()
    for obj in client.get_accounts():
        if (
            eligible_goa_account(obj)
            and goa_property(obj.get_account(), "Id") == request["goa_id"]
            and goa_property(obj.get_mail(), "EmailAddress") == request["email"]
        ):
            try:
                goa.Account.call_ensure_credentials_sync(obj.get_account(), None)
                token, _expires_in = goa.OAuth2Based.call_get_access_token_sync(
                    obj.get_oauth2_based(), None
                )
                return token
            except Exception as error:
                raise RuntimeError("GOA could not provide a mail access token; check Online Accounts") from error
    raise RuntimeError("This Google account is no longer available for Mail in Online Accounts")


def xoauth2_bytes(request):
    return f'user={request["email"]}\x01auth=Bearer {request["_goa_token"]}\x01\x01'.encode()


def authenticate_smtp(connection, request):
    if "goa_id" not in request:
        connection.login(request["email"], request["password"])
        return
    response = base64.b64encode(xoauth2_bytes(request)).decode("ascii")
    code, _detail = connection.docmd("AUTH XOAUTH2", response)
    if code == 334:
        code, _detail = connection.docmd("")
    if code != 235:
        raise smtplib.SMTPAuthenticationError(code, b"XOAUTH2 rejected")


def connect_imap(request):
    connection = imaplib.IMAP4_SSL(
        "imap.gmail.com", 993, ssl_context=ssl.create_default_context(), timeout=TIMEOUT
    )
    try:
        if "goa_id" in request:
            connection.authenticate("XOAUTH2", lambda _challenge: xoauth2_bytes(request))
        else:
            connection.login(request["email"], request["password"])
        return connection
    except BaseException:
        connection.shutdown()
        raise


def mailbox_name(connection, desired):
    if desired == "INBOX":
        return "INBOX"
    if desired not in SPECIAL:
        return encode_mailbox(desired)
    attribute = {
        "STARRED": b"\\Flagged",
        "SENT": b"\\Sent",
        "DRAFT": b"\\Drafts",
        "TRASH": b"\\Trash",
        "ALL": b"\\All",
    }[desired]
    for row in require_ok(connection.list(), "LIST"):
        if attribute.lower() in row.lower().split(b")", 1)[0]:
            # Gmail LIST response: (attributes) "/" "folder name".
            name = row.rsplit(b'"', 2)[-2] if row.endswith(b'"') else row.split()[-1]
            return name.decode("ascii")
    return SPECIAL[desired]


def select(connection, name, readonly=True):
    return require_ok(connection.select(name, readonly=readonly), f"SELECT {name}")


def ids(connection, *criteria):
    data = require_ok(connection.uid("SEARCH", None, *criteria), "SEARCH")
    return data[0].split() if data and data[0] else []


def metadata(value, name):
    match = re.search(rb"\b" + name + rb"\s+(\d+)", value)
    return match.group(1).decode("ascii") if match else None


def fetch_rows(connection, uid_set, fields):
    if not uid_set:
        return []
    data = require_ok(connection.uid("FETCH", b",".join(uid_set).decode(), fields), "FETCH")
    return [row for row in data if isinstance(row, tuple) and len(row) == 2]


def plain_text(part):
    try:
        return part.get_content()
    except (LookupError, UnicodeError, ValueError):
        return part.get_payload(decode=True).decode("utf-8", "replace")


def is_attachment(part):
    disposition = part.get_content_disposition()
    return bool(
        part.get_filename()
        or part.get("Content-ID")
        or disposition == "attachment"
        or (disposition == "inline" and part.get_content_maintype() == "image")
    )


def payload(part, path="", inline_attachments=False):
    headers = [{"name": key, "value": str(value)} for key, value in part.items()]
    body = {"data": None, "attachmentId": None}
    children = []
    if part.is_multipart() and not is_attachment(part):
        children = [
            payload(child, f"{path}.{index}" if path else str(index), inline_attachments)
            for index, child in enumerate(part.iter_parts())
        ]
    else:
        if is_attachment(part) and not inline_attachments:
            body["attachmentId"] = path
        else:
            content = part.get_payload(decode=True) or (part.as_bytes() if part.is_multipart() else b"")
            if part.get_content_maintype() == "text" and not is_attachment(part):
                content = plain_text(part).encode("utf-8")
            body["data"] = encoded(content)
    return {
        "filename": part.get_filename() or "",
        "mimeType": part.get_content_type(),
        "headers": headers,
        "body": body,
        "parts": children,
    }


def message_from_row(row, inline_attachments=False):
    meta, raw = row
    parsed = email.message_from_bytes(raw, policy=email.policy.default)
    msg_id = metadata(meta, b"X-GM-MSGID")
    thread_id = metadata(meta, b"X-GM-THRID")
    if not msg_id or not thread_id:
        raise RuntimeError("Gmail IMAP extensions are unavailable")
    labels = []
    flags = re.search(rb"\bFLAGS\s+\(([^)]*)\)", meta, re.I)
    if flags:
        values = flags.group(1).lower().split()
        if b"\\seen" not in values:
            labels.append("UNREAD")
        if b"\\flagged" in values:
            labels.append("STARRED")
        if b"\\draft" in values:
            labels.append("DRAFT")
    label_list = re.search(rb"\bX-GM-LABELS\s+\(([^)]*)\)", meta, re.I)
    if label_list:
        lower = label_list.group(1).lower()
        for source, label in [
            (b"\\inbox", "INBOX"),
            (b"\\sent", "SENT"),
            (b"\\trash", "TRASH"),
            (b"\\drafts", "DRAFT"),
        ]:
            if source in lower and label not in labels:
                labels.append(label)
    date_match = re.search(rb'\bINTERNALDATE\s+"([^"]+)"', meta, re.I)
    try:
        date = email.utils.parsedate_to_datetime(
            date_match.group(1).decode() if date_match else str(parsed.get("Date", ""))
        )
        timestamp = str(int(date.timestamp() * 1000))
    except (ValueError, TypeError, OverflowError):
        timestamp = "0"
    body = parsed.get_body(preferencelist=("plain", "html"))
    snippet = plain_text(body)[:160] if body else ""
    return {
        "id": msg_id,
        "threadId": thread_id,
        "labelIds": labels,
        "snippet": " ".join(snippet.split()),
        "payload": payload(parsed, inline_attachments=inline_attachments),
        "internalDate": timestamp,
    }


def fetch_messages(connection, uid_set, inline_attachments=False):
    results = []
    for offset in range(0, len(uid_set), 20):
        rows = fetch_rows(
            connection,
            uid_set[offset : offset + 20],
            "(X-GM-MSGID X-GM-THRID FLAGS X-GM-LABELS INTERNALDATE BODY.PEEK[])",
        )
        results.extend(message_from_row(row, inline_attachments) for row in rows)
    return sorted(results, key=lambda item: int(item["internalDate"]))


def locate(connection, message_id, writable=False):
    for label in ("ALL", "DRAFT", "TRASH"):
        select(connection, mailbox_name(connection, label), readonly=not writable)
        found = ids(connection, "X-GM-MSGID", message_id)
        if found:
            return found[0]
    raise RuntimeError("Message is no longer available; refresh the mailbox")


def list_threads(connection, request):
    label = request.get("label") or "ALL"
    count = int(select(connection, mailbox_name(connection, label))[0])
    query = request.get("query")
    limit = max(1, min(int(request.get("limit", 50)), 100))
    threads = []
    seen = set()
    # FETCH by message sequence number so large folders never produce an
    # unbounded SEARCH ALL response. Walk backward until enough unique threads
    # are found; one conversation can contain many messages.
    for end in range(count, 0, -500):
        start = max(1, end - 499)
        sequence_set = f"{start}:{end}"
        if query:
            matches = require_ok(
                connection.search(None, sequence_set, "X-GM-RAW", quoted(query)),
                "SEARCH",
            )
            sequence_ids = matches[0].split() if matches and matches[0] else []
            if not sequence_ids:
                continue
            sequence_set = b",".join(sequence_ids).decode("ascii")
        rows = require_ok(connection.fetch(sequence_set, "(X-GM-THRID)"), "FETCH")
        metadata_rows = [row[0] if isinstance(row, tuple) else row for row in rows if row]
        for meta in sorted(metadata_rows, key=lambda row: int(row.split(None, 1)[0]), reverse=True):
            thread_id = metadata(meta, b"X-GM-THRID")
            if thread_id and thread_id not in seen:
                seen.add(thread_id)
                threads.append({"id": thread_id})
                if len(threads) >= limit:
                    return {"threads": threads}
    return {"threads": threads}


def thread(connection, request):
    thread_id = request["id"]
    messages = {}
    for label in ("ALL", "DRAFT", "TRASH"):
        select(connection, mailbox_name(connection, label))
        found = ids(connection, "X-GM-THRID", thread_id)
        if found:
            for message in fetch_messages(connection, found):
                messages[message["id"]] = message
    return {"messages": sorted(messages.values(), key=lambda item: int(item["internalDate"]))}


def list_labels(connection):
    result = []
    special = {name.lower() for name in SPECIAL.values()}
    for row in require_ok(connection.list(), "LIST"):
        attributes = row.split(b")", 1)[0].lower()
        if any(
            attribute in attributes
            for attribute in (b"\\noselect", b"\\all", b"\\drafts", b"\\sent", b"\\trash", b"\\flagged", b"\\junk")
        ):
            continue
        name = row.rsplit(b'"', 2)[-2] if row.endswith(b'"') else row.split()[-1]
        name = decode_mailbox(name.decode("ascii", "replace"))
        if name.lower() != "inbox" and name.lower() not in special and not name.startswith(("[Gmail]/", "[GoogleMail]/")):
            result.append({"id": name, "name": name, "type": "user"})
    return result


def unread_counts(connection, labels):
    counts = []
    for label in labels:
        mailbox = mailbox_name(connection, label or "ALL")
        data = require_ok(connection.status(quoted(mailbox), "(UNSEEN)"), "STATUS")
        match = re.search(rb"\bUNSEEN\s+(\d+)\b", b" ".join(data or []), re.I)
        if match is None:
            raise RuntimeError("IMAP STATUS did not include an unread count")
        counts.append(int(match.group(1)))
    return counts


def raw_message(request):
    return decoded(request["raw"])


def attachment(connection, request):
    uid = locate(connection, request["id"])
    rows = fetch_rows(connection, [uid], "(BODY.PEEK[])")
    if not rows:
        raise RuntimeError("Attachment message is no longer available")
    part = email.message_from_bytes(rows[0][1], policy=email.policy.default)
    path = request["attachment_id"]
    if path:
        for index in path.split("."):
            children = list(part.iter_parts())
            part = children[int(index)]
    if not is_attachment(part):
        raise RuntimeError("The attachment changed; refresh the message")
    return encoded(part.get_payload(decode=True) or (part.as_bytes() if part.is_multipart() else b""))


def inline_images(connection, request):
    paths = request["attachment_ids"]
    if not paths:
        return {}
    uid = locate(connection, request["id"])
    rows = fetch_rows(connection, [uid], "(BODY.PEEK[])")
    if not rows:
        raise RuntimeError("Message is no longer available")
    message = email.message_from_bytes(rows[0][1], policy=email.policy.default)
    result = {}
    for path in paths:
        part = message
        for index in path.split("."):
            part = list(part.iter_parts())[int(index)]
        if part.get_content_maintype() != "image" or not part.get("Content-ID"):
            raise RuntimeError("An embedded image changed; refresh the message")
        result[path] = encoded(part.get_payload(decode=True) or b"")
    return result


def send(request):
    raw = raw_message(request)
    parsed = email.message_from_bytes(raw, policy=email.policy.SMTP)
    recipients = [
        address
        for _, address in email.utils.getaddresses(
            parsed.get_all("To", []) + parsed.get_all("Cc", []) + parsed.get_all("Bcc", [])
        )
    ]
    if not recipients:
        raise RuntimeError("Add a recipient before sending")
    if "Bcc" in parsed:
        del parsed["Bcc"]
    with smtplib.SMTP_SSL(
        "smtp.gmail.com", 465, context=ssl.create_default_context(), timeout=TIMEOUT
    ) as connection:
        connection.ehlo()
        authenticate_smtp(connection, request)
        connection.sendmail(request["email"], recipients, parsed.as_bytes())
    return None


def create_draft(connection, request):
    draft_box = mailbox_name(connection, "DRAFT")
    require_ok(
        connection.append(draft_box, "(\\Draft)", imaplib.Time2Internaldate(time.time()), raw_message(request)),
        "APPEND draft",
    )
    return None


def existing_draft(connection, request):
    draft_box = mailbox_name(connection, "DRAFT")
    select(connection, draft_box, readonly=False)
    found = ids(connection, "X-GM-MSGID", request["expected_message_id"])
    if not found or request["id"] != request["expected_message_id"]:
        raise RuntimeError("This draft changed elsewhere; reopen it before saving")
    old_uid = found[0]
    if request["send"]:
        send(request)
    else:
        create_draft(connection, request)
        select(connection, draft_box, readonly=False)
    # Move the old draft to Trash, avoiding a global EXPUNGE of other messages.
    require_ok(connection.uid("MOVE", old_uid, quoted(mailbox_name(connection, "TRASH"))), "MOVE old draft")
    return None


def dispatch(request):
    operation = request["operation"]
    if operation == "goa_accounts":
        return goa_accounts()
    if "goa_id" in request:
        request["_goa_token"] = goa_token(request)
    if operation == "send":
        return send(request)
    connection = connect_imap(request)
    try:
        if operation == "probe":
            capabilities = b" ".join(require_ok(connection.capability(), "CAPABILITY")).upper()
            if b"X-GM-EXT-1" not in capabilities:
                raise RuntimeError("Gmail IMAP extensions are unavailable for this account")
            select(connection, "INBOX")
            for required in ("ALL", "DRAFT", "TRASH"):
                select(connection, mailbox_name(connection, required))
            with smtplib.SMTP_SSL(
                "smtp.gmail.com", 465, context=ssl.create_default_context(), timeout=TIMEOUT
            ) as smtp:
                smtp.ehlo()
                authenticate_smtp(smtp, request)
            return None
        if operation == "labels":
            return list_labels(connection)
        if operation == "unread_counts":
            return unread_counts(connection, request["labels"])
        if operation == "list_threads":
            return list_threads(connection, request)
        if operation == "thread":
            return thread(connection, request)
        if operation == "threads":
            return [thread(connection, {"id": thread_id}) for thread_id in request["ids"]]
        if operation == "attachment":
            return attachment(connection, request)
        if operation == "inline_images":
            return inline_images(connection, request)
        if operation == "draft_for_message":
            select(connection, mailbox_name(connection, "DRAFT"))
            found = ids(connection, "X-GM-MSGID", request["id"])
            if not found:
                raise RuntimeError("This draft changed elsewhere; refresh the mailbox")
            messages = fetch_messages(connection, found, inline_attachments=True)
            return {"id": request["id"], "message": messages[0]}
        if operation == "create_draft":
            return create_draft(connection, request)
        if operation == "write_existing_draft":
            return existing_draft(connection, request)
        if operation == "archive_thread":
            select(connection, mailbox_name(connection, "ALL"), readonly=False)
            found = ids(connection, "X-GM-THRID", request["id"])
            if found:
                require_ok(connection.uid("STORE", b",".join(found).decode(), "-X-GM-LABELS", "(\\Inbox)"), "archive")
            return None
        if operation in ("set_unread", "set_starred", "trash"):
            uid = locate(connection, request["id"], writable=True)
            if operation == "trash":
                require_ok(connection.uid("MOVE", uid, quoted(mailbox_name(connection, "TRASH"))), "trash")
            else:
                flag = "\\Seen" if operation == "set_unread" else "\\Flagged"
                add = not request["value"] if operation == "set_unread" else request["value"]
                action = "+FLAGS.SILENT" if add else "-FLAGS.SILENT"
                require_ok(connection.uid("STORE", uid, action, f"({flag})"), operation)
            return None
        raise RuntimeError("Unsupported mail operation")
    finally:
        try:
            connection.logout()
        except (imaplib.IMAP4.error, OSError):
            pass


def main():
    try:
        request = json.load(sys.stdin)
        result = dispatch(request)
        json.dump({"ok": True, "result": result}, sys.stdout)
    except (imaplib.IMAP4.error, smtplib.SMTPException) as error:
        # Authentication failures may echo server text; do not forward it.
        json.dump({"ok": False, "error": f"Mail authentication or protocol error ({type(error).__name__})"}, sys.stdout)
    except (OSError, ssl.SSLError) as error:
        json.dump({"ok": False, "error": f"Mail network error ({type(error).__name__})"}, sys.stdout)
    except (RuntimeError, ValueError, KeyError, TypeError) as error:
        json.dump({"ok": False, "error": str(error)}, sys.stdout)


if __name__ == "__main__":
    main()
