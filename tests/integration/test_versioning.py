"""Integration tests for Phase 17: Object Versioning."""

import uuid

import pytest


@pytest.fixture
def vbucket(s3_client):
    """Create a versioned bucket, yield its name, then clean up."""
    name = f"versioning-test-{uuid.uuid4().hex[:8]}"
    s3_client.create_bucket(Bucket=name)
    s3_client.put_bucket_versioning(
        Bucket=name,
        VersioningConfiguration={"Status": "Enabled"},
    )
    yield name
    # Cleanup: delete all versions then bucket.
    response = s3_client.list_object_versions(Bucket=name)
    for v in response.get("Versions", []):
        s3_client.delete_object(
            Bucket=name, Key=v["Key"], VersionId=v["VersionId"]
        )
    for dm in response.get("DeleteMarkers", []):
        s3_client.delete_object(
            Bucket=name, Key=dm["Key"], VersionId=dm["VersionId"]
        )
    s3_client.delete_bucket(Bucket=name)


class TestBucketVersioningConfig:
    """PutBucketVersioning / GetBucketVersioning."""

    def test_get_versioning_unversioned(self, s3_client):
        bucket = f"test-unversioned-{uuid.uuid4().hex[:8]}"
        s3_client.create_bucket(Bucket=bucket)
        try:
            resp = s3_client.get_bucket_versioning(Bucket=bucket)
            assert "Status" not in resp
        finally:
            s3_client.delete_bucket(Bucket=bucket)

    def test_enable_versioning(self, s3_client):
        bucket = f"test-enable-ver-{uuid.uuid4().hex[:8]}"
        s3_client.create_bucket(Bucket=bucket)
        try:
            s3_client.put_bucket_versioning(
                Bucket=bucket,
                VersioningConfiguration={"Status": "Enabled"},
            )
            resp = s3_client.get_bucket_versioning(Bucket=bucket)
            assert resp["Status"] == "Enabled"
        finally:
            s3_client.delete_bucket(Bucket=bucket)

    def test_suspend_versioning(self, s3_client):
        bucket = f"test-suspend-ver-{uuid.uuid4().hex[:8]}"
        s3_client.create_bucket(Bucket=bucket)
        try:
            s3_client.put_bucket_versioning(
                Bucket=bucket,
                VersioningConfiguration={"Status": "Enabled"},
            )
            s3_client.put_bucket_versioning(
                Bucket=bucket,
                VersioningConfiguration={"Status": "Suspended"},
            )
            resp = s3_client.get_bucket_versioning(Bucket=bucket)
            assert resp["Status"] == "Suspended"
        finally:
            s3_client.delete_bucket(Bucket=bucket)


class TestVersionedPut:
    """PutObject on versioned buckets."""

    def test_put_returns_version_id(self, s3_client, vbucket):
        resp = s3_client.put_object(Bucket=vbucket, Key="obj", Body=b"v1")
        assert "VersionId" in resp
        assert resp["VersionId"] != "null"

    def test_put_twice_different_version_ids(self, s3_client, vbucket):
        r1 = s3_client.put_object(Bucket=vbucket, Key="obj", Body=b"v1")
        r2 = s3_client.put_object(Bucket=vbucket, Key="obj", Body=b"v2")
        assert r1["VersionId"] != r2["VersionId"]

    def test_get_returns_latest(self, s3_client, vbucket):
        s3_client.put_object(Bucket=vbucket, Key="obj", Body=b"v1")
        s3_client.put_object(Bucket=vbucket, Key="obj", Body=b"v2")
        resp = s3_client.get_object(Bucket=vbucket, Key="obj")
        assert resp["Body"].read() == b"v2"


class TestVersionedGet:
    """GetObject / HeadObject with versionId."""

    def test_get_specific_version(self, s3_client, vbucket):
        r1 = s3_client.put_object(Bucket=vbucket, Key="obj", Body=b"v1")
        s3_client.put_object(Bucket=vbucket, Key="obj", Body=b"v2")
        resp = s3_client.get_object(
            Bucket=vbucket, Key="obj", VersionId=r1["VersionId"]
        )
        assert resp["Body"].read() == b"v1"
        assert resp["VersionId"] == r1["VersionId"]

    def test_head_specific_version(self, s3_client, vbucket):
        r1 = s3_client.put_object(Bucket=vbucket, Key="obj", Body=b"data")
        resp = s3_client.head_object(
            Bucket=vbucket, Key="obj", VersionId=r1["VersionId"]
        )
        assert resp["ContentLength"] == 4
        assert resp["VersionId"] == r1["VersionId"]


class TestDeleteMarker:
    """Delete markers on versioned buckets."""

    def test_delete_creates_marker(self, s3_client, vbucket):
        s3_client.put_object(Bucket=vbucket, Key="obj", Body=b"data")
        resp = s3_client.delete_object(Bucket=vbucket, Key="obj")
        assert "VersionId" in resp
        assert resp.get("DeleteMarker", False) is True

    def test_get_after_delete_marker_returns_404(self, s3_client, vbucket):
        s3_client.put_object(Bucket=vbucket, Key="obj", Body=b"data")
        s3_client.delete_object(Bucket=vbucket, Key="obj")
        with pytest.raises(s3_client.exceptions.NoSuchKey):
            s3_client.get_object(Bucket=vbucket, Key="obj")

    def test_delete_with_version_id_permanent(self, s3_client, vbucket):
        r1 = s3_client.put_object(Bucket=vbucket, Key="obj", Body=b"data")
        s3_client.delete_object(
            Bucket=vbucket, Key="obj", VersionId=r1["VersionId"]
        )
        # Object should be gone (no versions remain).
        resp = s3_client.list_object_versions(Bucket=vbucket, Prefix="obj")
        versions = resp.get("Versions", [])
        assert len(versions) == 0

    def test_delete_delete_marker_undeletes(self, s3_client, vbucket):
        s3_client.put_object(Bucket=vbucket, Key="obj", Body=b"data")
        dm_resp = s3_client.delete_object(Bucket=vbucket, Key="obj")
        dm_vid = dm_resp["VersionId"]

        # Remove the delete marker.
        s3_client.delete_object(
            Bucket=vbucket, Key="obj", VersionId=dm_vid
        )

        # Object should be visible again.
        resp = s3_client.get_object(Bucket=vbucket, Key="obj")
        assert resp["Body"].read() == b"data"


class TestListObjectVersions:
    """ListObjectVersions with real version data."""

    def test_list_versions_returns_all(self, s3_client, vbucket):
        s3_client.put_object(Bucket=vbucket, Key="obj", Body=b"v1")
        s3_client.put_object(Bucket=vbucket, Key="obj", Body=b"v2")
        resp = s3_client.list_object_versions(Bucket=vbucket)
        versions = resp.get("Versions", [])
        assert len(versions) == 2
        # Newest first.
        assert versions[0]["IsLatest"] is True
        assert versions[1]["IsLatest"] is False
        # Both should have real version IDs.
        for v in versions:
            assert v["VersionId"] != "null"

    def test_list_versions_includes_delete_markers(self, s3_client, vbucket):
        s3_client.put_object(Bucket=vbucket, Key="obj", Body=b"data")
        s3_client.delete_object(Bucket=vbucket, Key="obj")
        resp = s3_client.list_object_versions(Bucket=vbucket)
        markers = resp.get("DeleteMarkers", [])
        assert len(markers) == 1
        assert markers[0]["IsLatest"] is True

    def test_list_versions_with_prefix(self, s3_client, vbucket):
        s3_client.put_object(Bucket=vbucket, Key="a/1", Body=b"1")
        s3_client.put_object(Bucket=vbucket, Key="b/1", Body=b"2")
        resp = s3_client.list_object_versions(Bucket=vbucket, Prefix="a/")
        versions = resp.get("Versions", [])
        assert len(versions) == 1
        assert versions[0]["Key"] == "a/1"


class TestCopyWithVersion:
    """CopyObject with source versionId."""

    def test_copy_specific_version(self, s3_client, vbucket):
        r1 = s3_client.put_object(Bucket=vbucket, Key="src", Body=b"v1")
        s3_client.put_object(Bucket=vbucket, Key="src", Body=b"v2")
        # Copy the old version to a new key.
        s3_client.copy_object(
            Bucket=vbucket,
            Key="dst",
            CopySource=f"{vbucket}/src?versionId={r1['VersionId']}",
        )
        resp = s3_client.get_object(Bucket=vbucket, Key="dst")
        assert resp["Body"].read() == b"v1"


class TestBatchDeleteVersioned:
    """DeleteObjects (batch) on versioned buckets."""

    def test_batch_delete_creates_markers_for_all(self, s3_client, vbucket):
        for i in range(3):
            s3_client.put_object(Bucket=vbucket, Key=f"f{i}.txt", Body=f"d{i}".encode())
        resp = s3_client.delete_objects(
            Bucket=vbucket,
            Delete={"Objects": [{"Key": f"f{i}.txt"} for i in range(3)], "Quiet": True},
        )
        assert resp.get("Errors", []) == []
        # All objects should be invisible (behind delete markers).
        listing = s3_client.list_objects_v2(Bucket=vbucket, Prefix="f")
        assert listing.get("Contents", []) == []

    def test_batch_delete_versions_preserved(self, s3_client, vbucket):
        for i in range(3):
            s3_client.put_object(Bucket=vbucket, Key=f"g{i}.txt", Body=f"d{i}".encode())
        s3_client.delete_objects(
            Bucket=vbucket,
            Delete={"Objects": [{"Key": f"g{i}.txt"} for i in range(3)], "Quiet": True},
        )
        # Versions and delete markers should still exist.
        resp = s3_client.list_object_versions(Bucket=vbucket, Prefix="g")
        versions = resp.get("Versions", [])
        markers = resp.get("DeleteMarkers", [])
        assert len(versions) == 3
        assert len(markers) == 3

    def test_batch_delete_with_version_ids(self, s3_client, vbucket):
        """DeleteObjects with VersionId hard-deletes specific versions (Ceph teardown pattern)."""
        # Create objects, then soft-delete them (creates delete markers).
        for i in range(3):
            s3_client.put_object(Bucket=vbucket, Key=f"h{i}.txt", Body=f"d{i}".encode())
        s3_client.delete_objects(
            Bucket=vbucket,
            Delete={"Objects": [{"Key": f"h{i}.txt"} for i in range(3)], "Quiet": True},
        )
        # Now list all versions + delete markers and hard-delete them by VersionId.
        resp = s3_client.list_object_versions(Bucket=vbucket, Prefix="h")
        to_delete = []
        for v in resp.get("Versions", []):
            to_delete.append({"Key": v["Key"], "VersionId": v["VersionId"]})
        for dm in resp.get("DeleteMarkers", []):
            to_delete.append({"Key": dm["Key"], "VersionId": dm["VersionId"]})
        assert len(to_delete) == 6  # 3 versions + 3 delete markers
        resp = s3_client.delete_objects(
            Bucket=vbucket, Delete={"Objects": to_delete, "Quiet": False}
        )
        assert resp.get("Errors", []) == []
        assert len(resp.get("Deleted", [])) == 6
        # Bucket should now be truly empty.
        resp = s3_client.list_object_versions(Bucket=vbucket, Prefix="h")
        assert resp.get("Versions", []) == []
        assert resp.get("DeleteMarkers", []) == []


class TestUnversionedBucketBackcompat:
    """Ensure unversioned buckets still work as before."""

    def test_put_no_version_id(self, s3_client):
        bucket = f"test-unver-{uuid.uuid4().hex[:8]}"
        s3_client.create_bucket(Bucket=bucket)
        try:
            resp = s3_client.put_object(Bucket=bucket, Key="obj", Body=b"data")
            # Unversioned: no VersionId or "null".
            vid = resp.get("VersionId")
            assert vid is None or vid == "null"
        finally:
            s3_client.delete_object(Bucket=bucket, Key="obj")
            s3_client.delete_bucket(Bucket=bucket)

    def test_delete_permanent(self, s3_client):
        bucket = f"test-unver-del-{uuid.uuid4().hex[:8]}"
        s3_client.create_bucket(Bucket=bucket)
        try:
            s3_client.put_object(Bucket=bucket, Key="obj", Body=b"data")
            s3_client.delete_object(Bucket=bucket, Key="obj")
            with pytest.raises(s3_client.exceptions.NoSuchKey):
                s3_client.get_object(Bucket=bucket, Key="obj")
        finally:
            try:
                s3_client.delete_object(Bucket=bucket, Key="obj")
            except Exception:
                pass
            s3_client.delete_bucket(Bucket=bucket)
