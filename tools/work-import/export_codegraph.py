#!/usr/bin/env python3
"""Capture the selected work tables from a pinned CodeGraph MariaDB store."""

import argparse
from contextlib import contextmanager
import base64
from collections import Counter
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import uuid
from urllib.parse import unquote, urlsplit

SOURCE_REVISION = "afe73560215bbe7593fc59451de5cc3237e3eeca"
MAX_ATTACHMENT = 1024 * 1024
MAX_RECORD_BYTES = 2 * MAX_ATTACHMENT
MAX_EXPORT = 256 * MAX_ATTACHMENT

# Qualifiers in these queries are fixed. The only interpolated predicate accepts normalized UUIDs.
# Private channels and live operational state are excluded in SQL before their payload leaves MariaDB.
TABLES = {
    "Projects": (
        "work_projects d", "d.id", "d.id",
        "id project_key name description goals_json version created_by created_by_kind created_session_id created_harness created_model created_host created_at updated_at",
    ),
    "Repositories": (
        "work_project_repositories d LEFT JOIN repositories r ON r.name=d.repository_name", "d.project_id", "d.project_id",
        "project_id repository_name role position",
    ),
    "Features": (
        "work_features d", "d.project_id", "d.id",
        "id number title description version created_by created_by_kind created_session_id created_harness created_model created_host created_at updated_at",
    ),
    "Items": (
        "work_items d", "d.project_id", "d.id",
        "id number feature_id item_type title description status priority sort_order labels repository_name version "
        "created_by created_by_kind created_session_id created_harness created_model created_host created_at updated_at",
    ),
    "Dependencies": (
        "work_item_dependencies d JOIN work_items i ON i.id=d.blocked_item_id", "i.project_id", "d.blocked_item_id",
        "blocked_item_id blocker_item_id created_at",
    ),
    "FeatureDocument": (
        "work_feature_documents d JOIN work_features f ON f.id=d.feature_id", "f.project_id", "d.feature_id",
        "id position title body created_by created_at updated_at",
    ),
    "Comment": (
        "work_item_comments d JOIN work_items i ON i.id=d.item_id", "i.project_id", "d.item_id",
        "id author author_kind author_session_id author_harness author_model author_host body external_id source_created_at created_at",
    ),
    "Link": (
        "work_item_links d JOIN work_items i ON i.id=d.item_id", "i.project_id", "d.item_id",
        "id kind url attempt_id historical created_by created_at",
    ),
    "Alias": (
        "work_item_aliases d JOIN work_items i ON i.id=d.item_id", "i.project_id", "d.item_id",
        "item_id alias created_by created_at",
    ),
    "Change": (
        "work_changes d", "d.project_id", "d.entity_id",
        "sequence project_key entity_kind entity_key kind actor_name actor_kind actor_session_id actor_harness actor_model actor_host changes created_at",
    ),
    "Handoff": (
        "work_handoffs d JOIN work_items i ON i.id=d.item_id", "i.project_id", "d.item_id",
        "id author author_kind author_session_id author_harness author_model author_host attempt_id done decisions_json open_issues_json corrections_json next_action branch commit_sha created_at",
    ),
    "Attempt": (
        "work_attempts d JOIN work_items i ON i.id=d.item_id", "i.project_id", "d.item_id",
        "id number holder session_id branch outcome reason handoff_id harness model host started_at ended_at",
    ),
    "Plan": (
        "work_plan_previews d", "d.project_id", "d.feature_id",
        "id identity plan_json versions_json created_at applied_at result_json",
    ),
    "Session": (
        "work_sessions d", "d.project_id", "d.project_id",
        "id identity harness model host repository_name branch intent started_at last_seen_at ended_at",
    ),
    "SessionClaim": (
        "work_session_claims d JOIN work_items i ON i.id=d.item_id", "i.project_id", "d.item_id",
        "item_id session_id claimed_at released_at",
    ),
    "ItemThread": (
        "work_messages d JOIN work_items i ON i.id=d.item_id", "i.project_id", "d.item_id",
        "sequence id kind state blocking in_reply_to body sender_name sender_kind sender_session_id "
        "resolved_by resolved_by_kind resolved_by_session_id sender_harness sender_model sender_host resolved_by_harness resolved_by_model resolved_by_host resolved_at created_at",
    ),
    "GitEvidence": (
        "work_github_evidence d JOIN work_items i ON i.id=d.item_id", "i.project_id", "d.item_id",
        "id kind evidence_key repository pr_number url external_id author_login source priority review_state "
        "location body attempt_id superseded created_at updated_at",
    ),
    "Attachment": (
        "work_item_attachments d JOIN work_items i ON i.id=d.item_id", "i.project_id", "d.item_id",
        "id external_id filename content_type size_bytes sha256 storage_path source_url skipped_reason created_by created_at",
    ),
    "Acceptance": (
        "work_items d", "d.project_id", "d.id",
        "id goal acceptance_json acceptance_report_json inherits_json touches_json plan_ref merged_awaiting_acceptance imported_json created_harness created_model created_host updated_at",
    ),
    "LegacyMapping": (
        "work_import_records d", "d.project_id", "d.entity_id",
        "source external_kind external_id entity_kind entity_id content_hash edges_json relations_json source_updated_at imported_at",
    ),
    "FeatureDefinition": (
        "work_features d", "d.project_id", "d.id",
        "id goals_json requirements_json acceptance_tests_json imported_json requirements_unreconciled created_harness created_model created_host updated_at",
    ),
}
EXTRA_PREDICATES = {
    "ItemThread": "d.channel='item' AND d.to_session_id IS NULL AND d.to_human=0",
    "Plan": "d.applied_at IS NOT NULL",
    "Change": "d.project_id IS NOT NULL AND d.entity_id IS NOT NULL",
    "SessionClaim": "d.released_at IS NOT NULL",
}

ID = re.compile(r"[A-Za-z0-9_-]{1,128}\Z")
CREDENTIAL_URL = re.compile(r"(?:https?|ssh)://[^/\s]+@", re.IGNORECASE)


def as_id(value):
    value = str(value)
    if ID.fullmatch(value):
        return value
    return "legacy-" + hashlib.sha256(value.encode()).hexdigest()[:48]


def compact(value):
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"), allow_nan=False)


def timestamp(value):
    if value is None:
        return 0
    value = datetime.fromisoformat(value)
    if value.tzinfo is None:
        value = value.replace(tzinfo=timezone.utc)
    milliseconds = value - datetime(1970, 1, 1, tzinfo=timezone.utc)
    return milliseconds // datetime.resolution // 1000


def stamp(row, capture):
    created = next((row.get(name) for name in ("created_at", "started_at", "claimed_at", "updated_at")
                    if row.get(name)), None)
    updated = next((row.get(name) for name in ("updated_at", "resolved_at", "applied_at", "ended_at",
                                               "released_at", "last_seen_at") if row.get(name)), created)
    created_at = timestamp(created) if created else capture
    updated_at = max(created_at, timestamp(updated))
    author = next((row.get(name) for name in ("created_by", "author", "actor_name", "sender_name", "author_login",
                                               "holder", "identity") if row.get(name)), None)
    author_kind = next((row.get(name) for name in ("created_by_kind", "author_kind", "actor_kind", "sender_kind")
                        if row.get(name)), None)
    session = next((row.get(name) for name in ("created_session_id", "author_session_id", "actor_session_id",
                                              "sender_session_id", "session_id") if row.get(name)), None)
    result = {"author": author, "authorKind": author_kind, "authorSession": as_id(session) if session else None,
              "createdAtMs": created_at, "updatedAtMs": updated_at}
    for key in ("harness", "model", "host"):
        value = next((row.get(prefix + key) for prefix in ("created_", "author_", "actor_", "sender_", "")
                      if row.get(prefix + key)), None)
        if value:
            result["author" + key.capitalize()] = value
    if row.get("version") is not None:
        result["sourceVersion"] = row["version"]
    return result


def digest_id(*fields):
    return "legacy-" + hashlib.sha256(compact(fields).encode()).hexdigest()[:48]


def list_json(value):
    result = json.loads(value) if value else []
    if not isinstance(result, list):
        raise ValueError("invalid list in the CodeGraph export source")
    return result


def read_attachment(row, root, missing):
    original = row.pop("storage_path", None)
    if not root or not original or not row.get("sha256"):
        missing["Attachment"] += 1
        row["verifiedBytes"] = False
        return
    try:
        selected = (root / original).resolve(strict=True)
        if not selected.is_relative_to(root) or not selected.is_file():
            raise ValueError("attachment escapes the supplied root")
        with selected.open("rb") as stream:
            data = stream.read(MAX_ATTACHMENT + 1)
        if len(data) > MAX_ATTACHMENT or hashlib.sha256(data).hexdigest() != row["sha256"].lower():
            raise ValueError("attachment digest or size")
        row["bytesBase64"] = base64.b64encode(data).decode("ascii")
        row["verifiedBytes"] = True
    except (OSError, ValueError):
        missing["Attachment"] += 1
        row["verifiedBytes"] = False


def sanitize(row):
    for key, value in row.items():
        if isinstance(value, str) and key.endswith("url"):
            row[key] = CREDENTIAL_URL.sub(lambda match: match.group(0).split("://")[0] + "://", value)
    return row


def convert(kind, project, subject, row, capture, attachment_root, missing):
    source_stamp = stamp(row, capture)
    if kind == "Projects":
        return {"kind": "Project", "id": project, "key": row["project_key"], "name": row["name"],
                "description": row["description"] or "", "goals": list_json(row["goals_json"]), "stamp": source_stamp}
    if kind == "Repositories":
        return {"kind": "Repository", "id": digest_id(project, row["repository_name"]), "projectId": project,
                "name": row["repository_name"], "remote": sanitize(row).get("repo_url"), "role": row["role"]}
    if kind == "Features":
        return {"kind": "Feature", "id": subject, "projectId": project, "number": row["number"],
                "title": row["title"], "description": row["description"] or "", "stamp": source_stamp}
    if kind == "Items":
        return {"kind": "Item", "id": subject, "projectId": project, "featureId": row["feature_id"],
                "number": row["number"], "itemType": row["item_type"], "title": row["title"],
                "description": row["description"] or "", "status": row["status"], "priority": row["priority"],
                "order": row["sort_order"], "labels": (row["labels"] or "").split(",") if row["labels"] else [],
                "repositoryName": row["repository_name"], "stamp": source_stamp}
    if kind == "Dependencies":
        return {"kind": "Dependency", "id": digest_id(subject, row["blocker_item_id"]), "projectId": project,
                "blocked": subject, "blocker": row["blocker_item_id"], "createdAtMs": source_stamp["createdAtMs"]}
    original_kind = kind
    if kind == "Change":
        changes = list_json(row["changes"])
        row["changes"] = compact([change for change in changes
                                  if change.get("field") != "claim_marker"])
    if kind == "Link":
        kind = categorize_link(row)
    if kind == "Attachment":
        read_attachment(row, attachment_root, missing)
    if kind == "Session":
        row["transcriptAvailability"] = "CodeGraph does not retain harness transcript bytes"
        missing["HarnessTranscript"] += 1
    if kind == "FeatureDefinition":
        kind = "Plan"
    default_key = (row["source"], row["external_kind"], row["external_id"]) if kind == "LegacyMapping" \
        else (row.get("item_id"), row.get("alias"), row.get("session_id"))
    key = row.get("id", row.get("sequence", default_key))
    return {"kind": "History", "id": digest_id(original_kind, key, subject), "projectId": project, "subjectId": subject,
            "historyKind": kind, "stamp": source_stamp, "evidence": sanitize(row)}


def categorize_link(row):
    try:
        url = urlsplit(row.get("url") or "")
    except ValueError:
        return "Link"
    segments = url.path.strip("/").split("/")
    if url.scheme in {"https", "http"} and url.hostname and len(segments) >= 5 \
            and segments[2] == "blob" and re.fullmatch(r"[a-fA-F0-9]{40}", segments[3]):
        row["remote"] = f"{url.scheme}://{url.hostname}/{'/'.join(segments[:2])}"
        row["commit"] = segments[3]
        row["path"] = unquote("/".join(segments[4:]))
        row["sourceSpan"] = url.fragment or None
        return "CodeReference"
    if row["kind"].lower() in {"branch", "pr", "commit", "pull_request", "pull-request", "review", "git"}:
        return "GitEvidence"
    return "Link"


def capture_query(projects):
    queries = [
        "SET SESSION time_zone='+00:00'",
        "SET SESSION max_statement_time=120",
        "START TRANSACTION WITH CONSISTENT SNAPSHOT, READ ONLY",
        "SELECT JSON_OBJECT('capture',CAST(UNIX_TIMESTAMP(CURRENT_TIMESTAMP(6))*1000 AS SIGNED))",
    ]
    for kind, (table, project, subject, names) in TABLES.items():
        columns = ",".join(f"'{name}',d.`{name}`" for name in names.split())
        if kind == "Repositories":
            columns += ",'repo_url',r.repo_url"
        condition = EXTRA_PREDICATES.get(kind, "1")
        if projects:
            literals = ",".join(f"'{project_id}'" for project_id in projects)
            condition = f"{condition} AND {project} IN ({literals})"
        queries.append(f"SELECT JSON_OBJECT('table','{kind}','project',{project},"
                       f"'subject',{subject},'record',JSON_OBJECT({columns})) FROM {table} WHERE {condition}")
    return ";\n".join(queries + ["COMMIT", ""])


@contextmanager
def bounded_source(command, query):
    def lines(process):
        total = 0
        count = 0
        while line := process.stdout.readline(MAX_RECORD_BYTES + 1):
            total += len(line)
            count += 1
            if len(line) > MAX_RECORD_BYTES or total > MAX_EXPORT or count > 500_001:
                raise ValueError("source exceeds export bounds; select a smaller project batch")
            yield line
        if process.wait(timeout=10):
            # Diagnostics are not returned to stdout because a client's defaults can include
            # credentials and source data.
            raise ValueError("the MariaDB client could not read the frozen work snapshot")

    with tempfile.TemporaryFile() as commands:
        commands.write(query.encode())
        commands.seek(0)
        with subprocess.Popen(command, stdin=commands, stdout=subprocess.PIPE,
                              stderr=subprocess.DEVNULL) as process:
            try:
                yield lines(process)
            finally:
                if process.poll() is None:
                    process.kill()


def export(args):
    projects = [str(uuid.UUID(project)) for project in args.project or []]
    if len(set(projects)) != len(projects):
        raise ValueError("a project can be selected only once")
    command = [args.client]
    if args.defaults_file:
        command.append(f"--defaults-extra-file={args.defaults_file}")
    command += ["--connect-timeout=10", "--batch", "--raw", "--skip-column-names", f"--database={args.database}"]
    attachment_root = args.attachments_root.resolve(strict=True) if args.attachments_root else None
    records, missing, counts = [], Counter(), Counter()
    exported_projects = set()
    with bounded_source(command, capture_query(projects)) as stream:
        header = next(stream, None)
        if header is None:
            raise ValueError("source snapshot did not return a capture boundary")
        capture = json.loads(header)["capture"]
        for line in stream:
            entry = json.loads(line)
            record = convert(entry["table"], entry["project"], entry["subject"], entry["record"],
                             capture, attachment_root, missing)
            if record["kind"] == "Project":
                exported_projects.add(record["id"])
            records.append(record)
            counts[record["kind"]] += 1
    if not exported_projects or projects and exported_projects != set(projects):
        raise ValueError("selected projects were not present in the source snapshot")
    order = {name: index for index, name in enumerate(
        ("Project", "Repository", "Feature", "Item", "Dependency", "History"))}
    records.sort(key=lambda row: (order[row["kind"]], row["id"]))
    body = bytearray()
    for row in records:
        data = compact(row).encode()
        if len(data) > MAX_RECORD_BYTES or len(body) + len(data) + 1 > MAX_EXPORT:
            raise ValueError("export exceeds the native bounds; select a smaller project batch")
        body.extend(data)
        body.append(10)
    header = {"format": "DevWorksCodeGraphWork", "version": 1, "namespace": args.namespace,
              "sourceRevision": SOURCE_REVISION, "capturedAtMs": capture, "coverageFromMs": 0,
              "recordsSha256": hashlib.sha256(body).hexdigest(), "counts": dict(counts),
              "missing": dict(missing), "selectedProjects": sorted(exported_projects)}
    with tempfile.NamedTemporaryFile(dir=args.output.parent, prefix=".work-export-", delete=False) as output:
        staged = Path(output.name)
        try:
            output.write(compact(header).encode() + b"\n")
            output.write(body)
            output.flush()
            os.fsync(output.fileno())
        except BaseException:
            staged.unlink()
            raise
    try:
        os.replace(staged, args.output)
    finally:
        staged.unlink(missing_ok=True)
    return header


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--client", default="mariadb")
    parser.add_argument("--defaults-file", type=Path)
    parser.add_argument("--database", required=True)
    parser.add_argument("--namespace", required=True, help="Stable identity of the source CodeGraph store")
    parser.add_argument("--attachments-root", type=Path)
    selection = parser.add_mutually_exclusive_group(required=True)
    selection.add_argument("--all", action="store_true")
    selection.add_argument("--project", action="append", help="Source project UUID. Repeat to select more than one.")
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    if not ID.fullmatch(args.namespace):
        parser.error("the source namespace must be an identifier")
    try:
        result = export(args)
    except (ValueError, OSError, subprocess.CalledProcessError, StopIteration) as error:
        if isinstance(error, subprocess.CalledProcessError):
            parser.exit(1, "CodeGraph export failed. Verify the pinned schema and read-only database connection.\n")
        parser.exit(1, f"CodeGraph export failed: {error}\n")
    print(compact({"counts": result["counts"], "missing": result["missing"], "sha256": result["recordsSha256"]}))


if __name__ == "__main__":
    main()
