"""Integration tests for TD-042: the requester is recorded as owner.

Objects (PutObject, CopyObject, CompleteMultipartUpload), delete markers,
buckets and multipart uploads (initiator) carry the username of the
authenticated user that created them, instead of a hard-coded `root`.
"""

import json
import os
import uuid

import boto3
import pytest
import requests
from botocore.auth import S3SigV4Auth
from botocore.awsrequest import AWSRequest
from botocore.config import Config
from botocore.credentials import Credentials


ROOT_KEY = os.environ.get("AWS_ACCESS_KEY_ID", "AKIA5B6BSHJA8CIHZSVG")
ROOT_SECRET = os.environ.get(
    "AWS_SECRET_ACCESS_KEY", "hQBDe6WnX9umjbSGCrld7YRUoYfaQUhUcJS/UAgv"
)
VERIFY = os.environ.get("AWS_CA_BUNDLE", True)


def _signed_admin(method, url, creds, data=None):
    headers = {}
    if data is not None:
        headers["Content-Type"] = "application/json"
        data = json.dumps(data)
    aws_req = AWSRequest(method=method, url=url, data=data or "", headers=headers)
    S3SigV4Auth(creds, "s3", "us-east-1").add_auth(aws_req)
    return requests.request(
        method,
        url,
        headers=dict(aws_req.headers),
        data=data,
        timeout=10,
        verify=VERIFY,
    )


def _client(endpoint_url, access_key, secret_key):
    kwargs = dict(
        endpoint_url=endpoint_url,
        aws_access_key_id=access_key,
        aws_secret_access_key=secret_key,
        region_name="us-east-1",
        config=Config(signature_version="s3v4"),
    )
    if endpoint_url.startswith("https://") and os.environ.get("AWS_CA_BUNDLE"):
        kwargs["verify"] = os.environ["AWS_CA_BUNDLE"]
    return boto3.client("s3", **kwargs)


@pytest.fixture
def root_s3(endpoint_url):
    return _client(endpoint_url, ROOT_KEY, ROOT_SECRET)


@pytest.fixture
def user(endpoint_url, root_s3):
    """A non-root user with S3FullAccess: yields (username, s3 client)."""
    root_creds = Credentials(ROOT_KEY, ROOT_SECRET)
    grants = _signed_admin("GET", f"{endpoint_url}/admin/grants", root_creds).json()
    grant_id = next(g["grant_id"] for g in grants if g["name"] == "S3FullAccess")

    username = f"owner-{uuid.uuid4().hex[:8]}"
    resp = _signed_admin(
        "POST", f"{endpoint_url}/admin/users", root_creds, data={"username": username}
    )
    assert resp.status_code == 201
    user_id = resp.json()["user_id"]
    _signed_admin(
        "PUT", f"{endpoint_url}/admin/users/{user_id}/grants/{grant_id}", root_creds
    )
    resp = _signed_admin(
        "POST",
        f"{endpoint_url}/admin/users/{user_id}/credentials",
        root_creds,
        data={"description": "owner tests"},
    )
    assert resp.status_code == 201
    cred = resp.json()
    yield username, _client(
        endpoint_url, cred["access_key_id"], cred["secret_access_key"]
    )
    _signed_admin(
        "DELETE", f"{endpoint_url}/admin/credentials/{cred['access_key_id']}", root_creds
    )
    _signed_admin("DELETE", f"{endpoint_url}/admin/users/{user_id}", root_creds)


def _empty_and_delete(s3, bucket):
    paginator = s3.get_paginator("list_object_versions")
    for page in paginator.paginate(Bucket=bucket):
        for v in page.get("Versions", []) + page.get("DeleteMarkers", []):
            s3.delete_object(Bucket=bucket, Key=v["Key"], VersionId=v["VersionId"])
    s3.delete_bucket(Bucket=bucket)


@pytest.fixture
def bucket(root_s3):
    name = f"owner-{uuid.uuid4().hex[:12]}"
    root_s3.create_bucket(Bucket=name)
    yield name
    _empty_and_delete(root_s3, name)


def _v2_owner(s3, bucket, key):
    resp = s3.list_objects_v2(Bucket=bucket, Prefix=key, FetchOwner=True)
    return resp["Contents"][0]["Owner"]


def _versions(s3, bucket, key, field="Versions"):
    resp = s3.list_object_versions(Bucket=bucket, Prefix=key)
    return resp.get(field, [])


# -- Objects --


def test_put_object_owner_is_the_requester(root_s3, user, bucket):
    username, user_s3 = user
    user_s3.put_object(Bucket=bucket, Key="k", Body=b"x")
    owner = _v2_owner(root_s3, bucket, "k")
    assert owner["ID"] == username
    assert owner["DisplayName"] == username


def test_root_put_object_owner_is_root(root_s3, bucket):
    root_s3.put_object(Bucket=bucket, Key="k", Body=b"x")
    assert _v2_owner(root_s3, bucket, "k")["ID"] == "root"


def test_list_v2_without_fetch_owner_has_no_owner(root_s3, bucket):
    root_s3.put_object(Bucket=bucket, Key="k", Body=b"x")
    resp = root_s3.list_objects_v2(Bucket=bucket, Prefix="k")
    assert "Owner" not in resp["Contents"][0]


def test_list_v1_returns_the_owner(root_s3, user, bucket):
    username, user_s3 = user
    user_s3.put_object(Bucket=bucket, Key="k", Body=b"x")
    resp = root_s3.list_objects(Bucket=bucket, Prefix="k")
    assert resp["Contents"][0]["Owner"]["ID"] == username


def test_copy_object_owner_is_the_copier(root_s3, user, bucket):
    username, user_s3 = user
    root_s3.put_object(Bucket=bucket, Key="src", Body=b"x")
    user_s3.copy_object(
        Bucket=bucket, Key="dst", CopySource={"Bucket": bucket, "Key": "src"}
    )
    assert _v2_owner(root_s3, bucket, "dst")["ID"] == username
    assert _v2_owner(root_s3, bucket, "src")["ID"] == "root"


def test_copy_onto_itself_with_replace_takes_the_requester(root_s3, user, bucket):
    username, user_s3 = user
    root_s3.put_object(Bucket=bucket, Key="k", Body=b"x")
    user_s3.copy_object(
        Bucket=bucket,
        Key="k",
        CopySource={"Bucket": bucket, "Key": "k"},
        MetadataDirective="REPLACE",
        Metadata={"a": "b"},
    )
    assert _v2_owner(root_s3, bucket, "k")["ID"] == username


def test_complete_multipart_owner_is_the_requester(root_s3, user, bucket):
    username, user_s3 = user
    up = user_s3.create_multipart_upload(Bucket=bucket, Key="mp")
    part = user_s3.upload_part(
        Bucket=bucket, Key="mp", UploadId=up["UploadId"], PartNumber=1, Body=b"x" * 10
    )
    user_s3.complete_multipart_upload(
        Bucket=bucket,
        Key="mp",
        UploadId=up["UploadId"],
        MultipartUpload={"Parts": [{"PartNumber": 1, "ETag": part["ETag"]}]},
    )
    assert _v2_owner(root_s3, bucket, "mp")["ID"] == username


def test_complete_by_another_user_takes_the_completer(root_s3, user, bucket):
    username, user_s3 = user
    up = root_s3.create_multipart_upload(Bucket=bucket, Key="mp")
    part = root_s3.upload_part(
        Bucket=bucket, Key="mp", UploadId=up["UploadId"], PartNumber=1, Body=b"x" * 10
    )
    user_s3.complete_multipart_upload(
        Bucket=bucket,
        Key="mp",
        UploadId=up["UploadId"],
        MultipartUpload={"Parts": [{"PartNumber": 1, "ETag": part["ETag"]}]},
    )
    assert _v2_owner(root_s3, bucket, "mp")["ID"] == username


def test_versions_keep_their_own_owner(root_s3, user, bucket):
    username, user_s3 = user
    root_s3.put_bucket_versioning(
        Bucket=bucket, VersioningConfiguration={"Status": "Enabled"}
    )
    root_s3.put_object(Bucket=bucket, Key="k", Body=b"1")
    user_s3.put_object(Bucket=bucket, Key="k", Body=b"2")
    versions = _versions(root_s3, bucket, "k")
    by_latest = {v["IsLatest"]: v["Owner"]["ID"] for v in versions}
    assert by_latest[True] == username
    assert by_latest[False] == "root"
    assert _v2_owner(root_s3, bucket, "k")["ID"] == username


def test_suspended_null_version_takes_the_writer(root_s3, user, bucket):
    username, user_s3 = user
    root_s3.put_bucket_versioning(
        Bucket=bucket, VersioningConfiguration={"Status": "Suspended"}
    )
    user_s3.put_object(Bucket=bucket, Key="k", Body=b"x")
    versions = _versions(root_s3, bucket, "k")
    assert versions[0]["VersionId"] == "null"
    assert versions[0]["Owner"]["ID"] == username


def test_presigned_put_owner_is_the_signer(root_s3, user, bucket):
    username, user_s3 = user
    url = user_s3.generate_presigned_url(
        "put_object", Params={"Bucket": bucket, "Key": "p"}, ExpiresIn=60
    )
    resp = requests.put(url, data=b"x", timeout=10, verify=VERIFY)
    assert resp.status_code == 200
    assert _v2_owner(root_s3, bucket, "p")["ID"] == username


# -- Delete markers --


def test_delete_marker_owner_is_the_requester(root_s3, user, bucket):
    username, user_s3 = user
    root_s3.put_bucket_versioning(
        Bucket=bucket, VersioningConfiguration={"Status": "Enabled"}
    )
    root_s3.put_object(Bucket=bucket, Key="k", Body=b"x")
    user_s3.delete_object(Bucket=bucket, Key="k")
    markers = _versions(root_s3, bucket, "k", "DeleteMarkers")
    assert markers[0]["Owner"]["ID"] == username


def test_batch_delete_marker_owner_is_the_requester(root_s3, user, bucket):
    username, user_s3 = user
    root_s3.put_bucket_versioning(
        Bucket=bucket, VersioningConfiguration={"Status": "Enabled"}
    )
    root_s3.put_object(Bucket=bucket, Key="k", Body=b"x")
    user_s3.delete_objects(Bucket=bucket, Delete={"Objects": [{"Key": "k"}]})
    markers = _versions(root_s3, bucket, "k", "DeleteMarkers")
    assert markers[0]["Owner"]["ID"] == username


def test_suspended_delete_marker_owner_is_the_requester(root_s3, user, bucket):
    username, user_s3 = user
    root_s3.put_bucket_versioning(
        Bucket=bucket, VersioningConfiguration={"Status": "Suspended"}
    )
    root_s3.put_object(Bucket=bucket, Key="k", Body=b"x")
    user_s3.delete_object(Bucket=bucket, Key="k")
    markers = _versions(root_s3, bucket, "k", "DeleteMarkers")
    assert markers[0]["Owner"]["ID"] == username


# -- Multipart initiator --


def test_list_parts_reports_the_initiator(root_s3, user, bucket):
    username, user_s3 = user
    up = user_s3.create_multipart_upload(Bucket=bucket, Key="mp")
    try:
        resp = root_s3.list_parts(Bucket=bucket, Key="mp", UploadId=up["UploadId"])
        assert resp["Owner"]["ID"] == username
        assert resp["Initiator"]["ID"] == username
    finally:
        root_s3.abort_multipart_upload(
            Bucket=bucket, Key="mp", UploadId=up["UploadId"]
        )


def test_list_multipart_uploads_reports_the_initiator(root_s3, user, bucket):
    username, user_s3 = user
    up = user_s3.create_multipart_upload(Bucket=bucket, Key="mp")
    try:
        resp = root_s3.list_multipart_uploads(Bucket=bucket)
        upload = resp["Uploads"][0]
        assert upload["Owner"]["ID"] == username
        assert upload["Initiator"]["ID"] == username
    finally:
        root_s3.abort_multipart_upload(
            Bucket=bucket, Key="mp", UploadId=up["UploadId"]
        )


# -- Buckets --


def _exported_bucket_owner(endpoint_url, name):
    resp = _signed_admin(
        "GET",
        f"{endpoint_url}/admin/export?sections=buckets",
        Credentials(ROOT_KEY, ROOT_SECRET),
    )
    assert resp.status_code == 200
    return next(b["owner"] for b in resp.json()["buckets"] if b["name"] == name)


def test_bucket_owner_is_the_creator(endpoint_url, user):
    username, user_s3 = user
    name = f"owner-{uuid.uuid4().hex[:12]}"
    user_s3.create_bucket(Bucket=name)
    try:
        assert _exported_bucket_owner(endpoint_url, name) == username
    finally:
        user_s3.delete_bucket(Bucket=name)


def test_root_bucket_owner_is_root(endpoint_url, bucket):
    assert _exported_bucket_owner(endpoint_url, bucket) == "root"
