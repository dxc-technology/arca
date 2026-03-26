"""Integration tests for Phase 21 — Object Lock (WORM Compliance).

Tests PutObjectLockConfiguration, GetObjectLockConfiguration,
PutObjectRetention, GetObjectRetention, PutObjectLegalHold,
GetObjectLegalHold, and enforcement of deletion blocking.
"""

import pytest
import uuid
from datetime import datetime, timedelta, timezone
from botocore.exceptions import ClientError


# -- Helpers --

def make_bucket(s3_client, prefix="lock"):
    """Create a uniquely named bucket."""
    name = f"{prefix}-{uuid.uuid4().hex[:8]}"
    s3_client.create_bucket(Bucket=name)
    return name


def cleanup_bucket(s3_client, bucket):
    """Best-effort cleanup of a bucket and its objects."""
    try:
        # List and delete all object versions
        paginator = s3_client.get_paginator("list_object_versions")
        for page in paginator.paginate(Bucket=bucket):
            for v in page.get("Versions", []):
                s3_client.delete_object(Bucket=bucket, Key=v["Key"], VersionId=v["VersionId"])
            for dm in page.get("DeleteMarkers", []):
                s3_client.delete_object(Bucket=bucket, Key=dm["Key"], VersionId=dm["VersionId"])
    except Exception:
        pass
    try:
        s3_client.delete_bucket(Bucket=bucket)
    except Exception:
        pass


# -- PutObjectLockConfiguration / GetObjectLockConfiguration --

class TestObjectLockConfig:
    def test_put_and_get_governance(self, s3_client):
        """Put Object Lock with GOVERNANCE default retention."""
        bucket = make_bucket(s3_client)
        try:
            s3_client.put_object_lock_configuration(
                Bucket=bucket,
                ObjectLockConfiguration={
                    "ObjectLockEnabled": "Enabled",
                    "Rule": {
                        "DefaultRetention": {"Mode": "GOVERNANCE", "Days": 30}
                    },
                },
            )
            resp = s3_client.get_object_lock_configuration(Bucket=bucket)
            config = resp["ObjectLockConfiguration"]
            assert config["ObjectLockEnabled"] == "Enabled"
            dr = config["Rule"]["DefaultRetention"]
            assert dr["Mode"] == "GOVERNANCE"
            assert dr["Days"] == 30
        finally:
            cleanup_bucket(s3_client, bucket)

    def test_put_enables_versioning(self, s3_client):
        """Object Lock auto-enables versioning."""
        bucket = make_bucket(s3_client)
        try:
            s3_client.put_object_lock_configuration(
                Bucket=bucket,
                ObjectLockConfiguration={
                    "ObjectLockEnabled": "Enabled",
                },
            )
            resp = s3_client.get_bucket_versioning(Bucket=bucket)
            assert resp.get("Status") == "Enabled"
        finally:
            cleanup_bucket(s3_client, bucket)

    def test_versioning_cannot_be_suspended_with_lock(self, s3_client):
        """Versioning cannot be suspended when Object Lock is enabled."""
        bucket = make_bucket(s3_client)
        try:
            s3_client.put_object_lock_configuration(
                Bucket=bucket,
                ObjectLockConfiguration={"ObjectLockEnabled": "Enabled"},
            )
            with pytest.raises(ClientError) as exc_info:
                s3_client.put_bucket_versioning(
                    Bucket=bucket,
                    VersioningConfiguration={"Status": "Suspended"},
                )
            # Should be rejected (InvalidArgument or similar)
            assert exc_info.value.response["ResponseMetadata"]["HTTPStatusCode"] == 400
        finally:
            cleanup_bucket(s3_client, bucket)

    def test_get_no_config(self, s3_client):
        """Get Object Lock on bucket without config returns 404."""
        bucket = make_bucket(s3_client)
        try:
            with pytest.raises(ClientError) as exc_info:
                s3_client.get_object_lock_configuration(Bucket=bucket)
            assert exc_info.value.response["Error"]["Code"] == "ObjectLockConfigurationNotFoundError"
        finally:
            cleanup_bucket(s3_client, bucket)

    def test_put_compliance_with_years(self, s3_client):
        """Put Object Lock with COMPLIANCE mode and years retention."""
        bucket = make_bucket(s3_client)
        try:
            s3_client.put_object_lock_configuration(
                Bucket=bucket,
                ObjectLockConfiguration={
                    "ObjectLockEnabled": "Enabled",
                    "Rule": {
                        "DefaultRetention": {"Mode": "COMPLIANCE", "Years": 1}
                    },
                },
            )
            resp = s3_client.get_object_lock_configuration(Bucket=bucket)
            dr = resp["ObjectLockConfiguration"]["Rule"]["DefaultRetention"]
            assert dr["Mode"] == "COMPLIANCE"
            assert dr["Years"] == 1
        finally:
            cleanup_bucket(s3_client, bucket)


# -- PutObjectRetention / GetObjectRetention --

class TestObjectRetention:
    def test_put_and_get_retention(self, s3_client):
        """Set retention on an object and read it back."""
        bucket = make_bucket(s3_client)
        try:
            # Enable versioning + Object Lock
            s3_client.put_object_lock_configuration(
                Bucket=bucket,
                ObjectLockConfiguration={"ObjectLockEnabled": "Enabled"},
            )
            s3_client.put_object(Bucket=bucket, Key="test.txt", Body=b"hello")
            retain_until = datetime.now(timezone.utc) + timedelta(days=30)
            s3_client.put_object_retention(
                Bucket=bucket,
                Key="test.txt",
                Retention={
                    "Mode": "GOVERNANCE",
                    "RetainUntilDate": retain_until,
                },
            )
            resp = s3_client.get_object_retention(Bucket=bucket, Key="test.txt")
            ret = resp["Retention"]
            assert ret["Mode"] == "GOVERNANCE"
            assert ret["RetainUntilDate"] is not None
        finally:
            cleanup_bucket(s3_client, bucket)

    def test_default_retention_on_put(self, s3_client):
        """Default retention from bucket config is applied on PutObject."""
        bucket = make_bucket(s3_client)
        try:
            s3_client.put_object_lock_configuration(
                Bucket=bucket,
                ObjectLockConfiguration={
                    "ObjectLockEnabled": "Enabled",
                    "Rule": {
                        "DefaultRetention": {"Mode": "GOVERNANCE", "Days": 10}
                    },
                },
            )
            s3_client.put_object(Bucket=bucket, Key="auto.txt", Body=b"data")
            # Check retention headers on HEAD
            head = s3_client.head_object(Bucket=bucket, Key="auto.txt")
            assert head.get("ObjectLockMode") == "GOVERNANCE"
            assert head.get("ObjectLockRetainUntilDate") is not None
        finally:
            cleanup_bucket(s3_client, bucket)

    def test_explicit_retention_headers_on_put(self, s3_client):
        """Explicit retention headers on PutObject override bucket default."""
        bucket = make_bucket(s3_client)
        try:
            s3_client.put_object_lock_configuration(
                Bucket=bucket,
                ObjectLockConfiguration={
                    "ObjectLockEnabled": "Enabled",
                    "Rule": {
                        "DefaultRetention": {"Mode": "GOVERNANCE", "Days": 10}
                    },
                },
            )
            retain_until = datetime.now(timezone.utc) + timedelta(days=90)
            s3_client.put_object(
                Bucket=bucket,
                Key="explicit.txt",
                Body=b"data",
                ObjectLockMode="COMPLIANCE",
                ObjectLockRetainUntilDate=retain_until,
            )
            head = s3_client.head_object(Bucket=bucket, Key="explicit.txt")
            assert head.get("ObjectLockMode") == "COMPLIANCE"
        finally:
            cleanup_bucket(s3_client, bucket)


# -- PutObjectLegalHold / GetObjectLegalHold --

class TestLegalHold:
    def test_put_and_get_legal_hold_on(self, s3_client):
        """Set legal hold ON and read it back."""
        bucket = make_bucket(s3_client)
        try:
            s3_client.put_object_lock_configuration(
                Bucket=bucket,
                ObjectLockConfiguration={"ObjectLockEnabled": "Enabled"},
            )
            s3_client.put_object(Bucket=bucket, Key="hold.txt", Body=b"data")
            s3_client.put_object_legal_hold(
                Bucket=bucket,
                Key="hold.txt",
                LegalHold={"Status": "ON"},
            )
            resp = s3_client.get_object_legal_hold(Bucket=bucket, Key="hold.txt")
            assert resp["LegalHold"]["Status"] == "ON"
        finally:
            cleanup_bucket(s3_client, bucket)

    def test_toggle_legal_hold_off(self, s3_client):
        """Toggle legal hold OFF."""
        bucket = make_bucket(s3_client)
        try:
            s3_client.put_object_lock_configuration(
                Bucket=bucket,
                ObjectLockConfiguration={"ObjectLockEnabled": "Enabled"},
            )
            s3_client.put_object(Bucket=bucket, Key="hold.txt", Body=b"data")
            s3_client.put_object_legal_hold(
                Bucket=bucket, Key="hold.txt", LegalHold={"Status": "ON"},
            )
            s3_client.put_object_legal_hold(
                Bucket=bucket, Key="hold.txt", LegalHold={"Status": "OFF"},
            )
            resp = s3_client.get_object_legal_hold(Bucket=bucket, Key="hold.txt")
            assert resp["LegalHold"]["Status"] == "OFF"
        finally:
            cleanup_bucket(s3_client, bucket)

    def test_legal_hold_requires_object_lock(self, s3_client):
        """Legal hold fails if Object Lock is not enabled on the bucket."""
        bucket = make_bucket(s3_client)
        try:
            s3_client.put_bucket_versioning(
                Bucket=bucket, VersioningConfiguration={"Status": "Enabled"},
            )
            s3_client.put_object(Bucket=bucket, Key="nolock.txt", Body=b"data")
            with pytest.raises(ClientError) as exc_info:
                s3_client.put_object_legal_hold(
                    Bucket=bucket, Key="nolock.txt", LegalHold={"Status": "ON"},
                )
            assert exc_info.value.response["ResponseMetadata"]["HTTPStatusCode"] == 400
        finally:
            cleanup_bucket(s3_client, bucket)


# -- Enforcement --

class TestEnforcement:
    def test_compliance_blocks_version_delete(self, s3_client):
        """COMPLIANCE retention blocks hard-deletion of a version."""
        bucket = make_bucket(s3_client)
        try:
            s3_client.put_object_lock_configuration(
                Bucket=bucket,
                ObjectLockConfiguration={"ObjectLockEnabled": "Enabled"},
            )
            s3_client.put_object(Bucket=bucket, Key="locked.txt", Body=b"secret")
            # Get version ID
            versions = s3_client.list_object_versions(Bucket=bucket)["Versions"]
            vid = versions[0]["VersionId"]
            # Set COMPLIANCE retention
            retain_until = datetime.now(timezone.utc) + timedelta(days=365)
            s3_client.put_object_retention(
                Bucket=bucket, Key="locked.txt",
                Retention={"Mode": "COMPLIANCE", "RetainUntilDate": retain_until},
            )
            # Attempt to delete specific version — should fail
            with pytest.raises(ClientError) as exc_info:
                s3_client.delete_object(Bucket=bucket, Key="locked.txt", VersionId=vid)
            assert exc_info.value.response["Error"]["Code"] == "AccessDenied"
        finally:
            cleanup_bucket(s3_client, bucket)

    def test_legal_hold_blocks_version_delete(self, s3_client):
        """Legal hold ON blocks hard-deletion of a version."""
        bucket = make_bucket(s3_client)
        try:
            s3_client.put_object_lock_configuration(
                Bucket=bucket,
                ObjectLockConfiguration={"ObjectLockEnabled": "Enabled"},
            )
            s3_client.put_object(Bucket=bucket, Key="held.txt", Body=b"data")
            versions = s3_client.list_object_versions(Bucket=bucket)["Versions"]
            vid = versions[0]["VersionId"]
            s3_client.put_object_legal_hold(
                Bucket=bucket, Key="held.txt", LegalHold={"Status": "ON"},
            )
            with pytest.raises(ClientError) as exc_info:
                s3_client.delete_object(Bucket=bucket, Key="held.txt", VersionId=vid)
            assert exc_info.value.response["Error"]["Code"] == "AccessDenied"
        finally:
            cleanup_bucket(s3_client, bucket)

    def test_delete_marker_creation_always_allowed(self, s3_client):
        """Creating a delete marker (no versionId) is always allowed."""
        bucket = make_bucket(s3_client)
        try:
            s3_client.put_object_lock_configuration(
                Bucket=bucket,
                ObjectLockConfiguration={"ObjectLockEnabled": "Enabled"},
            )
            s3_client.put_object(Bucket=bucket, Key="dm.txt", Body=b"data")
            retain_until = datetime.now(timezone.utc) + timedelta(days=365)
            s3_client.put_object_retention(
                Bucket=bucket, Key="dm.txt",
                Retention={"Mode": "COMPLIANCE", "RetainUntilDate": retain_until},
            )
            # Delete without versionId — creates delete marker, should succeed
            resp = s3_client.delete_object(Bucket=bucket, Key="dm.txt")
            assert resp["ResponseMetadata"]["HTTPStatusCode"] == 204
        finally:
            cleanup_bucket(s3_client, bucket)

    def test_governance_blocks_without_bypass(self, s3_client):
        """GOVERNANCE retention blocks delete without bypass header."""
        bucket = make_bucket(s3_client)
        try:
            s3_client.put_object_lock_configuration(
                Bucket=bucket,
                ObjectLockConfiguration={"ObjectLockEnabled": "Enabled"},
            )
            s3_client.put_object(Bucket=bucket, Key="gov.txt", Body=b"data")
            versions = s3_client.list_object_versions(Bucket=bucket)["Versions"]
            vid = versions[0]["VersionId"]
            retain_until = datetime.now(timezone.utc) + timedelta(days=365)
            s3_client.put_object_retention(
                Bucket=bucket, Key="gov.txt",
                Retention={"Mode": "GOVERNANCE", "RetainUntilDate": retain_until},
            )
            with pytest.raises(ClientError) as exc_info:
                s3_client.delete_object(Bucket=bucket, Key="gov.txt", VersionId=vid)
            assert exc_info.value.response["Error"]["Code"] == "AccessDenied"
        finally:
            cleanup_bucket(s3_client, bucket)

    def test_governance_allows_with_bypass(self, s3_client):
        """GOVERNANCE retention allows delete with bypass header."""
        bucket = make_bucket(s3_client)
        try:
            s3_client.put_object_lock_configuration(
                Bucket=bucket,
                ObjectLockConfiguration={"ObjectLockEnabled": "Enabled"},
            )
            s3_client.put_object(Bucket=bucket, Key="bypass.txt", Body=b"data")
            versions = s3_client.list_object_versions(Bucket=bucket)["Versions"]
            vid = versions[0]["VersionId"]
            retain_until = datetime.now(timezone.utc) + timedelta(days=365)
            s3_client.put_object_retention(
                Bucket=bucket, Key="bypass.txt",
                Retention={"Mode": "GOVERNANCE", "RetainUntilDate": retain_until},
            )
            # Delete with bypass header — should succeed
            resp = s3_client.delete_object(
                Bucket=bucket, Key="bypass.txt", VersionId=vid,
                BypassGovernanceRetention=True,
            )
            assert resp["ResponseMetadata"]["HTTPStatusCode"] == 204
        finally:
            cleanup_bucket(s3_client, bucket)
