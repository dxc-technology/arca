"""
Automated screenshot tool for Arca web console documentation.

Phase A: Seeds Arca with sample data using boto3 + Admin API.
Phase B: Captures screenshots using Playwright.
"""

import os
import json
import time

import boto3
import requests
from botocore.config import Config
from playwright.sync_api import sync_playwright

# Configuration from environment
ARCA_ENDPOINT = os.environ.get("ARCA_ENDPOINT", "http://arca:9000")
CONSOLE_URL = os.environ.get("CONSOLE_URL", "http://screenshots-console:3000")
ACCESS_KEY = os.environ.get("AWS_ACCESS_KEY_ID", "AKIAIOSFODNN7EXAMPLE")
SECRET_KEY = os.environ.get("AWS_SECRET_ACCESS_KEY", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY")
OUTPUT_DIR = "/app/output"

VIEWPORT = {"width": 1280, "height": 800}

# CSS to disable animations for deterministic captures
DISABLE_ANIMATIONS_CSS = """
*, *::before, *::after {
    animation-duration: 0s !important;
    animation-delay: 0s !important;
    transition-duration: 0s !important;
    transition-delay: 0s !important;
}
"""


def wait_for_arca():
    """Wait for Arca server to be ready."""
    print("Waiting for Arca server...")
    for i in range(60):
        try:
            resp = requests.get(f"{ARCA_ENDPOINT}/admin/health", timeout=2)
            if resp.status_code == 200:
                print("Arca server is ready.")
                return
        except requests.ConnectionError:
            pass
        time.sleep(1)
    raise RuntimeError("Arca server did not become ready in 60 seconds")


def wait_for_console():
    """Wait for console to be ready."""
    print("Waiting for console...")
    for i in range(60):
        try:
            resp = requests.get(CONSOLE_URL, timeout=2)
            if resp.status_code == 200:
                print("Console is ready.")
                return
        except requests.ConnectionError:
            pass
        time.sleep(1)
    raise RuntimeError("Console did not become ready in 60 seconds")


def create_s3_client():
    """Create a boto3 S3 client pointing at Arca."""
    return boto3.client(
        "s3",
        endpoint_url=ARCA_ENDPOINT,
        aws_access_key_id=ACCESS_KEY,
        aws_secret_access_key=SECRET_KEY,
        region_name="us-east-1",
        config=Config(s3={"addressing_style": "path"}),
    )


def signed_admin_request(method, path, body=None):
    """Make a SigV4-signed Admin API request. Returns the requests.Response."""
    from botocore.auth import S3SigV4Auth
    from botocore.credentials import Credentials
    from botocore.awsrequest import AWSRequest

    creds = Credentials(ACCESS_KEY, SECRET_KEY)
    url = f"{ARCA_ENDPOINT}{path}"
    data = json.dumps(body) if body is not None else None

    aws_request = AWSRequest(
        method=method,
        url=url,
        data=data,
        headers={"Content-Type": "application/json"} if data else {},
    )
    S3SigV4Auth(creds, "s3", "us-east-1").add_auth(aws_request)

    resp = requests.request(
        method=method,
        url=url,
        data=data,
        headers=dict(aws_request.headers),
    )
    return resp


def seed_data(s3):
    """Seed Arca with sample buckets and objects for screenshots."""
    print("\n=== Phase A: Seeding data ===")

    # Create buckets
    buckets = ["documents", "media", "backups", "logs"]
    for bucket in buckets:
        try:
            s3.create_bucket(Bucket=bucket)
            print(f"  Created bucket: {bucket}")
        except s3.exceptions.ClientError as e:
            if e.response["Error"]["Code"] == "BucketAlreadyOwnedByYou":
                print(f"  Bucket already exists: {bucket}")
            else:
                raise

    # Upload objects to documents
    s3.put_object(Bucket="documents", Key="reports/q1-2025.pdf", Body=b"\x00" * 245_000, ContentType="application/pdf")
    s3.put_object(Bucket="documents", Key="reports/q2-2025.pdf", Body=b"\x00" * 312_000, ContentType="application/pdf")
    s3.put_object(Bucket="documents", Key="contracts/vendor-a.pdf", Body=b"\x00" * 523_000, ContentType="application/pdf")
    s3.put_object(Bucket="documents", Key="readme.txt", Body=b"Welcome to Arca object storage.\nThis is a sample text file for documentation screenshots.", ContentType="text/plain")
    print("  Uploaded objects to documents/")

    # Upload objects to media
    s3.put_object(Bucket="media", Key="photos/vacation.jpg", Body=b"\xff\xd8" + b"\x00" * 2_500_000, ContentType="image/jpeg")
    s3.put_object(Bucket="media", Key="videos/demo.mp4", Body=b"\x00" * 15_000_000, ContentType="video/mp4")
    print("  Uploaded objects to media/")

    # Upload objects to backups
    s3.put_object(Bucket="backups", Key="db/2025-01-01.sql.gz", Body=b"\x1f\x8b" + b"\x00" * 5_000_000, ContentType="application/gzip")
    s3.put_object(Bucket="backups", Key="db/2025-02-01.sql.gz", Body=b"\x1f\x8b" + b"\x00" * 5_200_000, ContentType="application/gzip")
    print("  Uploaded objects to backups/")

    print("  Data seeding complete.")


def seed_versioning_data(s3):
    """Enable versioning on 'documents' and create version history for screenshots."""
    print("\n  Seeding versioning data...")

    # Enable versioning on the documents bucket
    s3.put_bucket_versioning(
        Bucket="documents",
        VersioningConfiguration={"Status": "Enabled"},
    )
    print("  Enabled versioning on documents")

    # Overwrite readme.txt twice to create version history (3 versions total)
    s3.put_object(
        Bucket="documents",
        Key="readme.txt",
        Body=b"Welcome to Arca object storage.\nVersion 2 - updated content.",
        ContentType="text/plain",
    )
    s3.put_object(
        Bucket="documents",
        Key="readme.txt",
        Body=b"Welcome to Arca object storage.\nVersion 3 - latest revision.",
        ContentType="text/plain",
    )
    print("  Created 3 versions of readme.txt")

    # Upload a temporary file and delete it to create a delete marker
    s3.put_object(
        Bucket="documents",
        Key="old-notes.txt",
        Body=b"These are old notes that will be deleted.",
        ContentType="text/plain",
    )
    s3.delete_object(Bucket="documents", Key="old-notes.txt")
    print("  Created delete marker for old-notes.txt")

    print("  Versioning seeding complete.")


def create_extra_credential():
    """Create an extra credential via Admin API with SigV4 signing."""
    print("  Creating extra credential via Admin API...")

    from botocore.auth import S3SigV4Auth
    from botocore.credentials import Credentials
    from botocore.awsrequest import AWSRequest

    creds = Credentials(ACCESS_KEY, SECRET_KEY)
    body = json.dumps({"description": "Backup Service", "admin": False})

    request = AWSRequest(
        method="POST",
        url=f"{ARCA_ENDPOINT}/admin/credentials",
        data=body,
        headers={"Content-Type": "application/json"},
    )
    S3SigV4Auth(creds, "s3", "us-east-1").add_auth(request)

    resp = requests.post(
        f"{ARCA_ENDPOINT}/admin/credentials",
        data=body,
        headers=dict(request.headers),
    )
    if resp.status_code == 201:
        result = resp.json()
        print(f"  Created credential: {result['access_key_id']}")
    else:
        print(f"  Warning: credential creation returned {resp.status_code}: {resp.text}")


def seed_notification_data(s3):
    """Configure a webhook notification on the 'logs' bucket for screenshots."""
    print("\n  Seeding notification data...")

    # Configure a webhook on the logs bucket via raw XML (to include Arca extensions)
    import hashlib
    from botocore.auth import S3SigV4Auth
    from botocore.credentials import Credentials
    from botocore.awsrequest import AWSRequest

    xml = """<?xml version="1.0" encoding="UTF-8"?>
<NotificationConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <TopicConfiguration>
    <Id>screenshot-webhook</Id>
    <Topic>http://example.com/webhook</Topic>
    <Event>s3:ObjectCreated:*</Event>
    <Event>s3:ObjectRemoved:*</Event>
    <Filter><S3Key>
      <FilterRule><Name>prefix</Name><Value>incoming/</Value></FilterRule>
    </S3Key></Filter>
  </TopicConfiguration>
</NotificationConfiguration>"""

    creds = Credentials(ACCESS_KEY, SECRET_KEY)
    url = f"{ARCA_ENDPOINT}/logs?notification"
    content_sha = hashlib.sha256(xml.encode()).hexdigest()
    aws_req = AWSRequest(method="PUT", url=url, data=xml, headers={
        "Content-Type": "application/xml",
        "x-amz-content-sha256": content_sha,
    })
    S3SigV4Auth(creds, "s3", "us-east-1").add_auth(aws_req)
    resp = requests.put(url, data=xml, headers=dict(aws_req.headers))
    if resp.status_code in (200, 204):
        print("  Configured webhook notification on logs bucket")
    else:
        print(f"  Warning: notification config returned {resp.status_code}: {resp.text}")

    print("  Notification seeding complete.")


def seed_rbac_data():
    """Seed RBAC data (users, teams, grants) via Admin API. Returns IDs for screenshots."""
    print("\n  Seeding RBAC data...")

    # Helper: create-or-get a user (idempotent across runs)
    def ensure_user(username, description):
        resp = signed_admin_request("POST", "/admin/users", {"username": username, "description": description})
        if resp.status_code == 201:
            user = resp.json()
            print(f"  Created user {username}: {user['user_id']}")
            return user["user_id"]
        if resp.status_code == 409:
            # Already exists — look up by listing users
            all_users = signed_admin_request("GET", "/admin/users").json()
            uid = next(u["user_id"] for u in all_users if u["username"] == username)
            print(f"  User {username} already exists: {uid}")
            return uid
        raise RuntimeError(f"Failed to create {username}: {resp.status_code} {resp.text}")

    # 1. Create user "alice"
    alice_user_id = ensure_user("alice", "Backend developer")

    # 2. Create user "bob"
    bob_user_id = ensure_user("bob", "Data analyst")

    # 3. Create a credential for alice (skip if she already has one)
    resp = signed_admin_request("GET", f"/admin/users/{alice_user_id}/credentials")
    alice_creds = resp.json() if resp.status_code == 200 else []
    if not alice_creds:
        resp = signed_admin_request("POST", f"/admin/users/{alice_user_id}/credentials", {"description": "Alice dev key"})
        assert resp.status_code == 201, f"Failed to create alice credential: {resp.status_code} {resp.text}"
        print(f"  Created credential for alice: {resp.json()['access_key_id']}")
    else:
        print(f"  Alice already has {len(alice_creds)} credential(s)")

    # 4. Create team "backend-devs"
    resp = signed_admin_request("POST", "/admin/teams", {"name": "backend-devs", "description": "Backend development team"})
    if resp.status_code == 201:
        team = resp.json()
        team_id = team["team_id"]
        print(f"  Created team backend-devs: {team_id}")
    elif resp.status_code == 409:
        all_teams = signed_admin_request("GET", "/admin/teams").json()
        team_id = next(t["team_id"] for t in all_teams if t["name"] == "backend-devs")
        print(f"  Team backend-devs already exists: {team_id}")
    else:
        raise RuntimeError(f"Failed to create team: {resp.status_code} {resp.text}")

    # 5. Add alice and bob as members of backend-devs (PUT is idempotent)
    resp = signed_admin_request("PUT", f"/admin/teams/{team_id}/members/{alice_user_id}")
    assert resp.status_code in (204, 409), f"Failed to add alice to team: {resp.status_code} {resp.text}"
    print(f"  Added alice to backend-devs")

    resp = signed_admin_request("PUT", f"/admin/teams/{team_id}/members/{bob_user_id}")
    assert resp.status_code in (204, 409), f"Failed to add bob to team: {resp.status_code} {resp.text}"
    print(f"  Added bob to backend-devs")

    # Get built-in grants
    resp = signed_admin_request("GET", "/admin/grants")
    assert resp.status_code == 200, f"Failed to list grants: {resp.status_code} {resp.text}"
    grants = resp.json()
    s3_full_access = next(g for g in grants if g["name"] == "S3FullAccess")
    s3_readonly = next(g for g in grants if g["name"] == "S3ReadOnlyAccess")

    # 6. Attach S3FullAccess to alice directly (PUT is idempotent)
    resp = signed_admin_request("PUT", f"/admin/users/{alice_user_id}/grants/{s3_full_access['grant_id']}")
    assert resp.status_code in (204, 409), f"Failed to attach grant to alice: {resp.status_code} {resp.text}"
    print(f"  Attached S3FullAccess to alice")

    # 7. Attach S3ReadOnlyAccess to backend-devs team (PUT is idempotent)
    resp = signed_admin_request("PUT", f"/admin/teams/{team_id}/grants/{s3_readonly['grant_id']}")
    assert resp.status_code in (204, 409), f"Failed to attach grant to team: {resp.status_code} {resp.text}"
    print(f"  Attached S3ReadOnlyAccess to backend-devs")

    print("  RBAC seeding complete.")

    return {
        "alice_user_id": alice_user_id,
        "bob_user_id": bob_user_id,
        "team_id": team_id,
        "s3_full_access_grant_id": s3_full_access["grant_id"],
        "s3_readonly_grant_id": s3_readonly["grant_id"],
    }


def screenshot(page, name):
    """Take a screenshot with a short stabilization delay."""
    page.wait_for_timeout(500)
    page.screenshot(path=f"{OUTPUT_DIR}/{name}")
    print(f"    Saved {name}")


def take_screenshots(rbac_ids):
    """Capture screenshots of the web console using Playwright."""
    print("\n=== Phase B: Taking screenshots ===")

    total = 32
    os.makedirs(OUTPUT_DIR, exist_ok=True)

    with sync_playwright() as p:
        browser = p.chromium.launch()
        context = browser.new_context(viewport=VIEWPORT)
        page = context.new_page()

        # Disable animations for deterministic captures
        page.add_style_tag(content=DISABLE_ANIMATIONS_CSS)

        # ----- 1. Login screen -----
        print(f"  1/{total} console-login.png")
        page.goto(CONSOLE_URL)
        page.wait_for_load_state("networkidle")
        # Fill endpoint field (visible because ARCA_ENDPOINT is not set on screenshots-console)
        page.fill('input[placeholder="http://localhost:9000"]', "http://arca:9000")
        screenshot(page, "console-login.png")

        # ----- 2. Dashboard (login + navigate) -----
        print(f"  2/{total} console-dashboard.png")
        page.fill('input[placeholder="AKIAIOSFODNN7EXAMPLE"]', ACCESS_KEY)
        page.fill('input[placeholder="wJalrXUtnFEMI/..."]', SECRET_KEY)
        page.click('button:has-text("Sign In")')
        # Wait for dashboard heading (admin login lands on dashboard)
        page.wait_for_selector('h2:has-text("Dashboard")', timeout=15000)
        page.wait_for_timeout(2000)
        screenshot(page, "console-dashboard.png")

        # ----- 3. Buckets view (shows encryption badges when encryption is enabled) -----
        print(f"  3/{total} console-buckets.png")
        page.goto(f"{CONSOLE_URL}#/buckets")
        page.wait_for_load_state("networkidle")
        page.wait_for_selector('.glass.rounded-xl.cursor-pointer', timeout=10000)
        page.wait_for_timeout(1000)
        screenshot(page, "console-buckets.png")

        # ----- 4. Bucket browser — documents (shows encryption shield in breadcrumb) -----
        print(f"  4/{total} console-bucket-browser.png")
        page.goto(f"{CONSOLE_URL}#/buckets/documents")
        page.wait_for_load_state("networkidle")
        page.wait_for_selector('[class*="cursor-pointer"][class*="border-b"]', timeout=10000)
        page.wait_for_timeout(1000)
        screenshot(page, "console-bucket-browser.png")

        # ----- 5. Object detail — readme.txt (shows Share button, encryption status) -----
        print(f"  5/{total} console-object-detail.png")
        page.locator('[class*="cursor-pointer"][class*="border-b"]:has-text("readme.txt")').click()
        page.wait_for_selector('text=Object Detail', timeout=10000)
        page.wait_for_timeout(500)
        screenshot(page, "console-object-detail.png")

        # ----- 6. Share modal — presigned URL generation -----
        print(f"  6/{total} console-share-modal.png")
        # Click Share button in the object detail panel
        page.click('button:has-text("Share")')
        page.wait_for_selector('h3:has-text("Share Object")', timeout=10000)
        page.wait_for_timeout(500)
        # Click "Generate Link" to produce a presigned URL
        page.click('button:has-text("Generate Link")')
        page.wait_for_selector('text=Presigned URL', timeout=10000)
        page.wait_for_timeout(500)
        screenshot(page, "console-share-modal.png")
        # Close the share modal
        page.click('.fixed button:has-text("Close")')
        page.wait_for_timeout(300)

        # ----- 7. Batch selection bar — select multiple objects -----
        print(f"  7/{total} console-batch-selection.png")
        # Full page reload to reset Alpine.js state (close detail panel, modals, etc.)
        page.goto(f"{CONSOLE_URL}#/buckets/documents")
        page.wait_for_load_state("networkidle")
        page.add_style_tag(content=DISABLE_ANIMATIONS_CSS)
        page.reload(wait_until="networkidle")
        page.wait_for_selector('[class*="cursor-pointer"][class*="border-b"]', timeout=10000)
        page.wait_for_timeout(500)
        # Select items using checkboxes
        checkboxes = page.locator('input[type="checkbox"]')
        count = checkboxes.count()
        # Select up to 3 items (skip the "select all" if present)
        selected = 0
        for i in range(count):
            if selected >= 3:
                break
            cb = checkboxes.nth(i)
            if cb.is_visible():
                cb.check()
                selected += 1
        page.wait_for_timeout(500)
        screenshot(page, "console-batch-selection.png")

        # ----- 8. Bucket settings — encryption toggle -----
        print(f"  8/{total} console-bucket-settings.png")
        page.goto(f"{CONSOLE_URL}#/buckets/documents/settings")
        page.wait_for_load_state("networkidle")
        page.wait_for_selector('h2:has-text("Bucket Settings")', timeout=10000)
        page.wait_for_timeout(1000)
        screenshot(page, "console-bucket-settings.png")

        # ----- 9. Versioning — show deleted objects -----
        print(f"  9/{total} console-show-deleted.png")
        page.goto(f"{CONSOLE_URL}#/buckets/documents")
        page.wait_for_load_state("networkidle")
        page.add_style_tag(content=DISABLE_ANIMATIONS_CSS)
        page.wait_for_selector('[class*="cursor-pointer"][class*="border-b"]', timeout=10000)
        page.wait_for_timeout(500)
        # Toggle "Show deleted" to reveal deleted objects
        page.click('button[title="Show deleted objects"]')
        page.wait_for_timeout(1500)
        screenshot(page, "console-show-deleted.png")

        # ----- 10. Versioning — version history panel -----
        print(f"  10/{total} console-version-history.png")
        # Click readme.txt to open object detail
        page.locator('[class*="cursor-pointer"][class*="border-b"]:has-text("readme.txt")').click()
        page.wait_for_selector('text=Object Detail', timeout=10000)
        page.wait_for_timeout(500)
        # Expand version history
        page.locator('button:has-text("Versions")').click()
        page.wait_for_timeout(1500)
        screenshot(page, "console-version-history.png")

        # ----- 11. Versioning — delete version modal -----
        print(f"  11/{total} console-delete-version-modal.png")
        # Click delete button on the oldest (last) version entry
        delete_btns = page.locator('button[title="Permanently delete this version"]')
        if delete_btns.count() > 1:
            delete_btns.last.click()
        else:
            delete_btns.first.click()
        page.wait_for_selector('h3:has-text("Delete Version")', timeout=10000)
        page.wait_for_timeout(500)
        screenshot(page, "console-delete-version-modal.png")
        # Close the modal by clicking the backdrop overlay (top-left corner)
        page.mouse.click(50, 50)
        page.wait_for_timeout(300)

        # ----- 12. Versioning — bucket settings with versioning enabled -----
        print(f"  12/{total} console-versioning-settings.png")
        page.goto(f"{CONSOLE_URL}#/buckets/documents/settings")
        page.wait_for_load_state("networkidle")
        page.wait_for_selector('text=Versioning Active', timeout=10000)
        page.wait_for_timeout(1000)
        screenshot(page, "console-versioning-settings.png")

        # ----- 13. Treemap — media bucket -----
        print(f"  13/{total} console-treemap.png")
        page.goto(f"{CONSOLE_URL}#/buckets/media")
        page.reload(wait_until="networkidle")
        page.wait_for_selector('[class*="cursor-pointer"][class*="border-b"]', timeout=10000)
        page.wait_for_timeout(500)
        page.click('[title="Treemap view"]')
        page.wait_for_timeout(1000)
        screenshot(page, "console-treemap.png")

        # ----- 14. Credentials view -----
        print(f"  14/{total} console-credentials.png")
        page.goto(f"{CONSOLE_URL}#/credentials")
        page.wait_for_load_state("networkidle")
        page.wait_for_selector('.glass.rounded-xl:has-text("Active")', timeout=10000)
        page.wait_for_timeout(1000)
        screenshot(page, "console-credentials.png")

        # ----- 15. Create credential -----
        print(f"  15/{total} console-credential-created.png")
        page.click('button:has-text("Create Credential")')
        page.wait_for_timeout(500)
        page.fill('input[placeholder="My application"]', "CI/CD Pipeline")
        page.locator('.fixed form button[type="submit"]').click()
        page.wait_for_selector('text=Credential Created', timeout=10000)
        page.wait_for_timeout(500)
        screenshot(page, "console-credential-created.png")

        # ----- 16. Users list view -----
        print(f"  16/{total} console-users.png")
        page.goto(f"{CONSOLE_URL}#/users")
        page.wait_for_load_state("networkidle")
        page.wait_for_selector('.glass.glass-hover.rounded-xl', timeout=10000)
        page.wait_for_timeout(1000)
        screenshot(page, "console-users.png")

        # ----- 17. User detail — alice, Credentials tab -----
        print(f"  17/{total} console-user-detail.png")
        page.goto(f"{CONSOLE_URL}#/users/{rbac_ids['alice_user_id']}")
        page.wait_for_load_state("networkidle")
        page.wait_for_selector('button:has-text("Credentials")', timeout=10000)
        page.wait_for_timeout(1000)
        # Credentials tab is the default active tab
        screenshot(page, "console-user-detail.png")

        # ----- 18. User detail — alice, Direct Grants tab -----
        print(f"  18/{total} console-user-grants.png")
        page.click('button:has-text("Direct Grants")')
        page.wait_for_timeout(1000)
        screenshot(page, "console-user-grants.png")

        # ----- 19. User detail — alice, Effective Grants tab -----
        print(f"  19/{total} console-user-effective.png")
        page.click('button:has-text("Effective Grants")')
        page.wait_for_timeout(1000)
        screenshot(page, "console-user-effective.png")

        # ----- 20. Teams list view -----
        print(f"  20/{total} console-teams.png")
        page.goto(f"{CONSOLE_URL}#/teams")
        page.wait_for_load_state("networkidle")
        page.wait_for_selector('h2:has-text("Teams")', timeout=10000)
        page.wait_for_timeout(1500)
        screenshot(page, "console-teams.png")

        # ----- 21. Team detail — backend-devs, Members tab -----
        print(f"  21/{total} console-team-detail.png")
        page.goto(f"{CONSOLE_URL}#/teams/{rbac_ids['team_id']}")
        page.wait_for_load_state("networkidle")
        page.wait_for_selector('button:has-text("Members")', timeout=10000)
        page.wait_for_timeout(1000)
        # Members tab is the default active tab
        screenshot(page, "console-team-detail.png")

        # ----- 22. Grants list view -----
        print(f"  22/{total} console-grants.png")
        page.goto(f"{CONSOLE_URL}#/grants")
        page.wait_for_load_state("networkidle")
        page.wait_for_selector('h2:has-text("Grants")', timeout=10000)
        page.wait_for_timeout(1500)
        screenshot(page, "console-grants.png")

        # ----- 23. Grant detail — S3FullAccess -----
        print(f"  23/{total} console-grant-detail.png")
        page.goto(f"{CONSOLE_URL}#/grants/{rbac_ids['s3_full_access_grant_id']}")
        page.wait_for_load_state("networkidle")
        page.wait_for_selector('text=Statement Preview', timeout=10000)
        page.wait_for_timeout(1500)
        screenshot(page, "console-grant-detail.png")

        # ----- 24. Audit Log -----
        print(f"  24/{total} console-audit-log.png")
        page.goto(f"{CONSOLE_URL}#/audit")
        page.wait_for_load_state("networkidle")
        page.wait_for_selector('h2:has-text("Audit Log")', timeout=10000)
        page.wait_for_timeout(2000)
        screenshot(page, "console-audit-log.png")

        # ----- 25. Monitoring -----
        print(f"  25/{total} console-monitoring.png")
        page.goto(f"{CONSOLE_URL}#/monitoring")
        page.wait_for_load_state("networkidle")
        page.wait_for_selector('h2:has-text("Monitoring")', timeout=10000)
        page.wait_for_timeout(2000)
        screenshot(page, "console-monitoring.png")

        # ----- 26. Settings -----
        print(f"  26/{total} console-settings.png")
        page.goto(f"{CONSOLE_URL}#/settings")
        page.wait_for_load_state("networkidle")
        page.wait_for_selector('h2:has-text("Settings")', timeout=10000)
        page.wait_for_timeout(1500)
        screenshot(page, "console-settings.png")

        # ----- 27. Bucket settings — event notifications card (with configured webhook) -----
        print(f"  27/{total} console-event-notifications.png")
        page.goto(f"{CONSOLE_URL}#/buckets/logs/settings")
        page.wait_for_load_state("networkidle")
        page.wait_for_selector('text=Event Notifications', timeout=10000)
        page.wait_for_timeout(1000)
        screenshot(page, "console-event-notifications.png")

        # ----- 28. Add notification modal (connector type selector — all 13 active) -----
        print(f"  28/{total} console-notification-modal.png")
        page.click('button:has-text("Add Notification")')
        page.wait_for_selector('h3:has-text("Add Event Notification")', timeout=10000)
        page.wait_for_timeout(500)
        screenshot(page, "console-notification-modal.png")

        # ----- 29. Notification modal — Webhook connector form (default, most common) -----
        print(f"  29/{total} console-notification-webhook.png")
        # Webhook is selected by default; populate representative values so the
        # form renders with visible data.
        page.fill('input[placeholder="http://example.com/webhook"]', "https://ops.example.com/arca-events")
        page.fill('input[placeholder="Bearer authentication token"]', "secret-token")
        page.wait_for_timeout(500)
        screenshot(page, "console-notification-webhook.png")

        # ----- 30. Notification modal — SMTP connector form -----
        print(f"  30/{total} console-notification-smtp.png")
        # Select the SMTP tile in the picker
        page.click('button:has(span:has-text("SMTP"))')
        page.wait_for_selector('label:has-text("SMTP Destination")', timeout=10000)
        # Populate representative values so the form renders meaningfully
        page.fill('input[placeholder="smtp://hostname:25 or smtps://hostname:465"]', "smtp://mail.example.com:587")
        page.fill('input[placeholder="ops@example.com"]', "ops@example.com")
        page.fill('input[placeholder="arca-notify@example.com"]', "arca@example.com")
        page.fill('input[placeholder="Arca S3 Notification: {event}"]', "Arca S3 alert")
        page.wait_for_timeout(500)
        screenshot(page, "console-notification-smtp.png")

        # ----- 31. Notification modal — gRPC connector form -----
        print(f"  31/{total} console-notification-grpc.png")
        page.click('button:has(span:has-text("gRPC"))')
        page.wait_for_selector('label:has-text("gRPC Destination")', timeout=10000)
        page.fill('input[placeholder="http://hostname:50051 or https://hostname:50051"]', "https://grpc.example.com:50051")
        page.fill('input[placeholder="Bearer authentication token"]', "secret-token")
        page.fill('input[placeholder="example.com"]', "grpc.example.com")
        page.wait_for_timeout(500)
        screenshot(page, "console-notification-grpc.png")
        # Close the modal
        page.mouse.click(50, 50)
        page.wait_for_timeout(300)

        # ----- 32. Notification event log -----
        print(f"  32/{total} console-notification-events.png")
        page.goto(f"{CONSOLE_URL}#/notifications")
        page.wait_for_load_state("networkidle")
        page.wait_for_selector('h2:has-text("Notification Events")', timeout=10000)
        page.wait_for_timeout(2000)
        screenshot(page, "console-notification-events.png")

        browser.close()

    print("\n  All screenshots saved to", OUTPUT_DIR)


def main():
    wait_for_arca()
    wait_for_console()

    s3 = create_s3_client()
    seed_data(s3)
    seed_versioning_data(s3)
    seed_notification_data(s3)
    create_extra_credential()
    rbac_ids = seed_rbac_data()
    take_screenshots(rbac_ids)

    # List output files
    files = sorted(os.listdir(OUTPUT_DIR))
    png_files = [f for f in files if f.endswith(".png")]
    print(f"\n=== Done: {len(png_files)} screenshots ===")
    for f in png_files:
        size = os.path.getsize(os.path.join(OUTPUT_DIR, f))
        print(f"  {f} ({size:,} bytes)")


if __name__ == "__main__":
    main()
