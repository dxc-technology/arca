"""
Automated screenshot tool for Arca web console documentation.

Phase A: Seeds Arca with sample data using boto3 + Admin API.
Phase B: Captures screenshots using Playwright.
"""

import os
import hashlib
import hmac
import json
import time
from datetime import datetime, timezone, timedelta

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

        # 1. Login screen
        print("  1/8 console-login.png")
        page.goto(CONSOLE_URL)
        page.wait_for_load_state("networkidle")
        # Fill endpoint field (visible because ARCA_ENDPOINT is not set on screenshots-console)
        endpoint_input = page.locator('input[placeholder*="endpoint" i], input[placeholder*="Endpoint" i], input[id*="endpoint" i]')
        if endpoint_input.count() > 0:
            endpoint_input.first.fill("http://arca:9000")
        else:
            # Try a more generic approach — find the first input
            inputs = page.locator("input")
            if inputs.count() >= 3:
                inputs.nth(0).fill("http://arca:9000")
        page.screenshot(path=f"{OUTPUT_DIR}/console-login.png")

        # 2. Dashboard
        print("  2/8 console-dashboard.png")
        # Fill credentials and login
        access_key_input = page.locator('input[placeholder*="Access Key" i], input[id*="access" i]')
        secret_key_input = page.locator('input[placeholder*="Secret Key" i], input[id*="secret" i], input[type="password"]')
        if access_key_input.count() > 0:
            access_key_input.first.fill(ACCESS_KEY)
        if secret_key_input.count() > 0:
            secret_key_input.first.fill(SECRET_KEY)
        # Click login button
        login_button = page.locator('button:has-text("Connect"), button:has-text("Login"), button:has-text("Sign In"), button[type="submit"]')
        if login_button.count() > 0:
            login_button.first.click()
        page.wait_for_load_state("networkidle")
        # Wait a moment for dashboard data to load
        page.wait_for_timeout(2000)
        page.screenshot(path=f"{OUTPUT_DIR}/console-dashboard.png")

        # 3. Buckets view
        print("  3/8 console-buckets.png")
        buckets_nav = page.locator('a:has-text("Buckets"), button:has-text("Buckets"), [data-nav="buckets"]')
        if buckets_nav.count() > 0:
            buckets_nav.first.click()
        page.wait_for_load_state("networkidle")
        page.wait_for_timeout(1000)
        page.screenshot(path=f"{OUTPUT_DIR}/console-buckets.png")

        # 4. Bucket browser — documents
        print("  4/8 console-bucket-browser.png")
        doc_bucket = page.locator('text="documents"').first
        doc_bucket.click()
        page.wait_for_load_state("networkidle")
        page.wait_for_timeout(1000)
        page.screenshot(path=f"{OUTPUT_DIR}/console-bucket-browser.png")

        # 5. Object detail — readme.txt
        print("  5/8 console-object-detail.png")
        readme = page.locator('text="readme.txt"').first
        readme.click()
        page.wait_for_timeout(1000)
        page.screenshot(path=f"{OUTPUT_DIR}/console-object-detail.png")

        # 6. Treemap — navigate to media bucket
        print("  6/8 console-treemap.png")
        # Go back to buckets
        buckets_nav = page.locator('a:has-text("Buckets"), button:has-text("Buckets"), [data-nav="buckets"]')
        if buckets_nav.count() > 0:
            buckets_nav.first.click()
        page.wait_for_load_state("networkidle")
        page.wait_for_timeout(500)
        media_bucket = page.locator('text="media"').first
        media_bucket.click()
        page.wait_for_load_state("networkidle")
        page.wait_for_timeout(500)
        # Toggle treemap
        treemap_toggle = page.locator('button:has-text("Treemap"), button:has-text("treemap"), [title*="treemap" i], [data-action="treemap"]')
        if treemap_toggle.count() > 0:
            treemap_toggle.first.click()
            page.wait_for_timeout(1000)
        page.screenshot(path=f"{OUTPUT_DIR}/console-treemap.png")

        # 7. Credentials view
        print("  7/8 console-credentials.png")
        creds_nav = page.locator('a:has-text("Credentials"), button:has-text("Credentials"), [data-nav="credentials"]')
        if creds_nav.count() > 0:
            creds_nav.first.click()
        page.wait_for_load_state("networkidle")
        page.wait_for_timeout(1000)
        page.screenshot(path=f"{OUTPUT_DIR}/console-credentials.png")

        # 8. Create credential
        print("  8/8 console-credential-created.png")
        create_btn = page.locator('button:has-text("Create"), button:has-text("Add"), button:has-text("New")')
        if create_btn.count() > 0:
            create_btn.first.click()
            page.wait_for_timeout(500)
            # Fill description
            desc_input = page.locator('input[placeholder*="description" i], input[placeholder*="Description" i], input[id*="description" i]')
            if desc_input.count() > 0:
                desc_input.first.fill("CI/CD Pipeline")
            # Submit
            submit_btn = page.locator('button:has-text("Create"), button:has-text("Save"), button:has-text("Submit"), button[type="submit"]')
            if submit_btn.count() > 0:
                # Click the submit button inside the modal (last match is usually the modal one)
                submit_btn.last.click()
            page.wait_for_timeout(1500)
        page.screenshot(path=f"{OUTPUT_DIR}/console-credential-created.png")

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
