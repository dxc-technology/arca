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


def screenshot(page, name):
    """Take a screenshot with a short stabilization delay."""
    page.wait_for_timeout(500)
    page.screenshot(path=f"{OUTPUT_DIR}/{name}")
    print(f"    Saved {name}")


def take_screenshots():
    """Capture screenshots of the web console using Playwright."""
    print("\n=== Phase B: Taking screenshots ===")

    os.makedirs(OUTPUT_DIR, exist_ok=True)

    with sync_playwright() as p:
        browser = p.chromium.launch()
        context = browser.new_context(viewport=VIEWPORT)
        page = context.new_page()

        # Disable animations for deterministic captures
        page.add_style_tag(content=DISABLE_ANIMATIONS_CSS)

        # ----- 1. Login screen -----
        print("  1/8 console-login.png")
        page.goto(CONSOLE_URL)
        page.wait_for_load_state("networkidle")
        # Fill endpoint field (visible because ARCA_ENDPOINT is not set on screenshots-console)
        page.fill('input[placeholder="http://localhost:9000"]', "http://arca:9000")
        screenshot(page, "console-login.png")

        # ----- 2. Dashboard (login + navigate) -----
        print("  2/8 console-dashboard.png")
        page.fill('input[placeholder="AKIAIOSFODNN7EXAMPLE"]', ACCESS_KEY)
        page.fill('input[placeholder="wJalrXUtnFEMI/..."]', SECRET_KEY)
        page.click('button:has-text("Sign In")')
        # Wait for dashboard heading (admin login lands on dashboard)
        page.wait_for_selector('h2:has-text("Dashboard")', timeout=15000)
        page.wait_for_timeout(2000)
        screenshot(page, "console-dashboard.png")

        # ----- 3. Buckets view -----
        print("  3/8 console-buckets.png")
        # Navigate via hash (most reliable — avoids Alpine.js click timing issues)
        page.goto(f"{CONSOLE_URL}#/buckets")
        page.wait_for_load_state("networkidle")
        # Wait for bucket cards to appear — they contain bucket names set via x-text
        page.wait_for_selector('.glass.rounded-xl.cursor-pointer', timeout=10000)
        page.wait_for_timeout(1000)
        screenshot(page, "console-buckets.png")

        # ----- 4. Bucket browser — documents -----
        print("  4/8 console-bucket-browser.png")
        page.goto(f"{CONSOLE_URL}#/buckets/documents")
        page.wait_for_load_state("networkidle")
        # Wait for file/folder rows to render (hover:bg-white rows are file/dir items)
        page.wait_for_selector('[class*="cursor-pointer"][class*="border-b"]', timeout=10000)
        page.wait_for_timeout(1000)
        screenshot(page, "console-bucket-browser.png")

        # ----- 5. Object detail — readme.txt -----
        print("  5/8 console-object-detail.png")
        # Click the readme.txt file row
        page.locator('[class*="cursor-pointer"][class*="border-b"]:has-text("readme.txt")').click()
        # Wait for detail panel to appear
        page.wait_for_selector('text=Object Detail', timeout=10000)
        page.wait_for_timeout(500)
        screenshot(page, "console-object-detail.png")

        # ----- 6. Treemap — media bucket -----
        print("  6/8 console-treemap.png")
        # Navigate to media bucket — reload page to clear the detail panel from step 5
        page.goto(f"{CONSOLE_URL}#/buckets/media")
        page.reload(wait_until="networkidle")
        page.wait_for_selector('[class*="cursor-pointer"][class*="border-b"]', timeout=10000)
        page.wait_for_timeout(500)
        # Click the treemap toggle button (has title="Treemap view")
        page.click('[title="Treemap view"]')
        page.wait_for_timeout(1000)
        screenshot(page, "console-treemap.png")

        # ----- 7. Credentials view -----
        print("  7/8 console-credentials.png")
        page.goto(f"{CONSOLE_URL}#/credentials")
        page.wait_for_load_state("networkidle")
        # Wait for credential cards
        page.wait_for_selector('.glass.rounded-xl:has-text("Active")', timeout=10000)
        page.wait_for_timeout(1000)
        screenshot(page, "console-credentials.png")

        # ----- 8. Create credential -----
        print("  8/8 console-credential-created.png")
        # Click "+ Create Credential" button
        page.click('button:has-text("Create Credential")')
        page.wait_for_timeout(500)
        # Fill description in the modal
        page.fill('input[placeholder="My application"]', "CI/CD Pipeline")
        # Click Create button inside the modal form
        page.locator('.fixed button:has-text("Create")').click()
        # Wait for the "Credential Created" success view with the secret key
        page.wait_for_selector('text=Credential Created', timeout=10000)
        page.wait_for_timeout(500)
        screenshot(page, "console-credential-created.png")

        browser.close()

    print("\n  All screenshots saved to", OUTPUT_DIR)


def main():
    wait_for_arca()
    wait_for_console()

    s3 = create_s3_client()
    seed_data(s3)
    create_extra_credential()
    take_screenshots()

    # List output files
    files = sorted(os.listdir(OUTPUT_DIR))
    png_files = [f for f in files if f.endswith(".png")]
    print(f"\n=== Done: {len(png_files)} screenshots ===")
    for f in png_files:
        size = os.path.getsize(os.path.join(OUTPUT_DIR, f))
        print(f"  {f} ({size:,} bytes)")


if __name__ == "__main__":
    main()
