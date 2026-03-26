"""Integration tests for Phase 20 — Lifecycle Rules.

Tests PutBucketLifecycleConfiguration, GetBucketLifecycleConfiguration,
DeleteBucketLifecycleConfiguration with various rule types and filters.
"""

import pytest
import uuid
from botocore.exceptions import ClientError


# -- Helpers --

def make_bucket(s3_client, prefix="lifecycle"):
    """Create a uniquely named bucket and return its name."""
    name = f"{prefix}-{uuid.uuid4().hex[:8]}"
    s3_client.create_bucket(Bucket=name)
    return name


def cleanup_bucket(s3_client, bucket):
    """Delete all objects and the bucket."""
    try:
        resp = s3_client.list_objects_v2(Bucket=bucket)
        for obj in resp.get("Contents", []):
            s3_client.delete_object(Bucket=bucket, Key=obj["Key"])
    except Exception:
        pass
    try:
        s3_client.delete_bucket(Bucket=bucket)
    except Exception:
        pass


LIFECYCLE_EXPIRATION_RULE = {
    "Rules": [
        {
            "ID": "expire-old-logs",
            "Status": "Enabled",
            "Filter": {"Prefix": "logs/"},
            "Expiration": {"Days": 90},
        }
    ]
}


# -- PutBucketLifecycleConfiguration --

class TestPutLifecycle:
    def test_put_expiration_rule(self, s3_client):
        """Put a simple expiration rule and verify it's accepted."""
        bucket = make_bucket(s3_client)
        try:
            s3_client.put_bucket_lifecycle_configuration(
                Bucket=bucket,
                LifecycleConfiguration=LIFECYCLE_EXPIRATION_RULE,
            )
        finally:
            cleanup_bucket(s3_client, bucket)

    def test_put_abort_multipart_rule(self, s3_client):
        """Put an abort incomplete multipart upload rule."""
        bucket = make_bucket(s3_client)
        try:
            s3_client.put_bucket_lifecycle_configuration(
                Bucket=bucket,
                LifecycleConfiguration={
                    "Rules": [
                        {
                            "ID": "abort-uploads",
                            "Status": "Enabled",
                            "Filter": {"Prefix": ""},
                            "AbortIncompleteMultipartUpload": {
                                "DaysAfterInitiation": 7
                            },
                        }
                    ]
                },
            )
        finally:
            cleanup_bucket(s3_client, bucket)

    def test_put_noncurrent_version_expiration(self, s3_client):
        """Put a noncurrent version expiration rule."""
        bucket = make_bucket(s3_client)
        try:
            s3_client.put_bucket_lifecycle_configuration(
                Bucket=bucket,
                LifecycleConfiguration={
                    "Rules": [
                        {
                            "ID": "cleanup-versions",
                            "Status": "Enabled",
                            "Filter": {"Prefix": ""},
                            "NoncurrentVersionExpiration": {
                                "NoncurrentDays": 30
                            },
                        }
                    ]
                },
            )
        finally:
            cleanup_bucket(s3_client, bucket)

    def test_put_multiple_rules(self, s3_client):
        """Put a configuration with multiple rules."""
        bucket = make_bucket(s3_client)
        try:
            s3_client.put_bucket_lifecycle_configuration(
                Bucket=bucket,
                LifecycleConfiguration={
                    "Rules": [
                        {
                            "ID": "expire-logs",
                            "Status": "Enabled",
                            "Filter": {"Prefix": "logs/"},
                            "Expiration": {"Days": 30},
                        },
                        {
                            "ID": "abort-uploads",
                            "Status": "Enabled",
                            "Filter": {"Prefix": ""},
                            "AbortIncompleteMultipartUpload": {
                                "DaysAfterInitiation": 7
                            },
                        },
                    ]
                },
            )
        finally:
            cleanup_bucket(s3_client, bucket)

    def test_put_disabled_rule(self, s3_client):
        """Put a disabled rule."""
        bucket = make_bucket(s3_client)
        try:
            s3_client.put_bucket_lifecycle_configuration(
                Bucket=bucket,
                LifecycleConfiguration={
                    "Rules": [
                        {
                            "ID": "disabled-rule",
                            "Status": "Disabled",
                            "Filter": {"Prefix": ""},
                            "Expiration": {"Days": 365},
                        }
                    ]
                },
            )
        finally:
            cleanup_bucket(s3_client, bucket)

    def test_put_tag_filter(self, s3_client):
        """Put a rule with tag-based filtering."""
        bucket = make_bucket(s3_client)
        try:
            s3_client.put_bucket_lifecycle_configuration(
                Bucket=bucket,
                LifecycleConfiguration={
                    "Rules": [
                        {
                            "ID": "expire-temp",
                            "Status": "Enabled",
                            "Filter": {
                                "Tag": {"Key": "status", "Value": "temporary"},
                            },
                            "Expiration": {"Days": 1},
                        }
                    ]
                },
            )
        finally:
            cleanup_bucket(s3_client, bucket)

    def test_put_and_filter(self, s3_client):
        """Put a rule with And filter (prefix + tag)."""
        bucket = make_bucket(s3_client)
        try:
            s3_client.put_bucket_lifecycle_configuration(
                Bucket=bucket,
                LifecycleConfiguration={
                    "Rules": [
                        {
                            "ID": "and-rule",
                            "Status": "Enabled",
                            "Filter": {
                                "And": {
                                    "Prefix": "data/",
                                    "Tags": [
                                        {"Key": "env", "Value": "staging"},
                                    ],
                                },
                            },
                            "Expiration": {"Days": 60},
                        }
                    ]
                },
            )
        finally:
            cleanup_bucket(s3_client, bucket)

    def test_put_overwrites_existing(self, s3_client):
        """Putting lifecycle config twice should overwrite the first."""
        bucket = make_bucket(s3_client)
        try:
            s3_client.put_bucket_lifecycle_configuration(
                Bucket=bucket,
                LifecycleConfiguration=LIFECYCLE_EXPIRATION_RULE,
            )
            # Overwrite with different rules
            new_rules = {
                "Rules": [
                    {
                        "ID": "new-rule",
                        "Status": "Enabled",
                        "Filter": {"Prefix": "data/"},
                        "Expiration": {"Days": 7},
                    }
                ]
            }
            s3_client.put_bucket_lifecycle_configuration(
                Bucket=bucket, LifecycleConfiguration=new_rules
            )
            resp = s3_client.get_bucket_lifecycle_configuration(Bucket=bucket)
            assert len(resp["Rules"]) == 1
            assert resp["Rules"][0]["ID"] == "new-rule"
        finally:
            cleanup_bucket(s3_client, bucket)

    def test_put_nonexistent_bucket(self, s3_client):
        """Put lifecycle on a nonexistent bucket should return NoSuchBucket."""
        with pytest.raises(ClientError) as exc_info:
            s3_client.put_bucket_lifecycle_configuration(
                Bucket="nonexistent-lifecycle-bucket",
                LifecycleConfiguration=LIFECYCLE_EXPIRATION_RULE,
            )
        assert exc_info.value.response["Error"]["Code"] == "NoSuchBucket"

    def test_put_all_actions(self, s3_client):
        """Put a rule with all three action types."""
        bucket = make_bucket(s3_client)
        try:
            s3_client.put_bucket_lifecycle_configuration(
                Bucket=bucket,
                LifecycleConfiguration={
                    "Rules": [
                        {
                            "ID": "all-actions",
                            "Status": "Enabled",
                            "Filter": {"Prefix": ""},
                            "Expiration": {"Days": 365},
                            "NoncurrentVersionExpiration": {
                                "NoncurrentDays": 90
                            },
                            "AbortIncompleteMultipartUpload": {
                                "DaysAfterInitiation": 7
                            },
                        }
                    ]
                },
            )
        finally:
            cleanup_bucket(s3_client, bucket)


# -- GetBucketLifecycleConfiguration --

class TestGetLifecycle:
    def test_get_returns_saved_config(self, s3_client):
        """Get should return the exact configuration that was put."""
        bucket = make_bucket(s3_client)
        try:
            s3_client.put_bucket_lifecycle_configuration(
                Bucket=bucket,
                LifecycleConfiguration=LIFECYCLE_EXPIRATION_RULE,
            )
            resp = s3_client.get_bucket_lifecycle_configuration(Bucket=bucket)
            rules = resp["Rules"]
            assert len(rules) == 1
            assert rules[0]["ID"] == "expire-old-logs"
            assert rules[0]["Status"] == "Enabled"
            assert rules[0]["Expiration"]["Days"] == 90
        finally:
            cleanup_bucket(s3_client, bucket)

    def test_get_no_lifecycle_config(self, s3_client):
        """Get on bucket with no lifecycle should return NoSuchLifecycleConfiguration."""
        bucket = make_bucket(s3_client)
        try:
            with pytest.raises(ClientError) as exc_info:
                s3_client.get_bucket_lifecycle_configuration(Bucket=bucket)
            assert exc_info.value.response["Error"]["Code"] == "NoSuchLifecycleConfiguration"
        finally:
            cleanup_bucket(s3_client, bucket)

    def test_get_nonexistent_bucket(self, s3_client):
        """Get lifecycle on a nonexistent bucket should return NoSuchBucket."""
        with pytest.raises(ClientError) as exc_info:
            s3_client.get_bucket_lifecycle_configuration(
                Bucket="nonexistent-lifecycle-bucket-get"
            )
        assert exc_info.value.response["Error"]["Code"] == "NoSuchBucket"

    def test_get_multiple_rules(self, s3_client):
        """Get should return all rules that were put."""
        bucket = make_bucket(s3_client)
        try:
            config = {
                "Rules": [
                    {
                        "ID": "rule-1",
                        "Status": "Enabled",
                        "Filter": {"Prefix": "logs/"},
                        "Expiration": {"Days": 30},
                    },
                    {
                        "ID": "rule-2",
                        "Status": "Disabled",
                        "Filter": {"Prefix": ""},
                        "AbortIncompleteMultipartUpload": {
                            "DaysAfterInitiation": 7
                        },
                    },
                ]
            }
            s3_client.put_bucket_lifecycle_configuration(
                Bucket=bucket, LifecycleConfiguration=config
            )
            resp = s3_client.get_bucket_lifecycle_configuration(Bucket=bucket)
            rules = resp["Rules"]
            assert len(rules) == 2
            rule_ids = {r["ID"] for r in rules}
            assert rule_ids == {"rule-1", "rule-2"}
        finally:
            cleanup_bucket(s3_client, bucket)


# -- DeleteBucketLifecycleConfiguration --

class TestDeleteLifecycle:
    def test_delete_existing(self, s3_client):
        """Delete lifecycle config, then get should return 404."""
        bucket = make_bucket(s3_client)
        try:
            s3_client.put_bucket_lifecycle_configuration(
                Bucket=bucket,
                LifecycleConfiguration=LIFECYCLE_EXPIRATION_RULE,
            )
            s3_client.delete_bucket_lifecycle(Bucket=bucket)

            with pytest.raises(ClientError) as exc_info:
                s3_client.get_bucket_lifecycle_configuration(Bucket=bucket)
            assert exc_info.value.response["Error"]["Code"] == "NoSuchLifecycleConfiguration"
        finally:
            cleanup_bucket(s3_client, bucket)

    def test_delete_nonexistent_is_idempotent(self, s3_client):
        """Delete lifecycle on a bucket with no config should succeed (204)."""
        bucket = make_bucket(s3_client)
        try:
            # Should not raise
            s3_client.delete_bucket_lifecycle(Bucket=bucket)
        finally:
            cleanup_bucket(s3_client, bucket)

    def test_delete_nonexistent_bucket(self, s3_client):
        """Delete lifecycle on a nonexistent bucket should return NoSuchBucket."""
        with pytest.raises(ClientError) as exc_info:
            s3_client.delete_bucket_lifecycle(
                Bucket="nonexistent-lifecycle-bucket-del"
            )
        assert exc_info.value.response["Error"]["Code"] == "NoSuchBucket"
