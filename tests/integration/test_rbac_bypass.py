"""Integration tests: a non-root user must not reach objects through side doors.

Each class covers an operation that reads or destroys objects other than the
one in the request path, so the per-request check of the auth middleware is
not enough and the handler must authorize every object it touches:

- CopyObject / UploadPartCopy read the `x-amz-copy-source` object;
- `POST /admin/archive` reads every key in its body;
- DeleteObjects deletes every key in its body;
- `x-amz-bypass-governance-retention` needs `s3:BypassGovernanceRetention`.

`TestPercentEncodedPaths` covers the request path itself: the router decodes
bucket and key, so the policy must be evaluated on the decoded names.

Refusals must not reveal whether the bucket or key exists.
"""

import uuid
from datetime import datetime, timedelta, timezone

import pytest
from botocore.exceptions import ClientError

from rbac_helpers import (
    allow,
    own_bucket_statements,
    restricted_user,
    s3_client_for,
    signed_request,
)


def _empty_and_delete(s3, bucket):
    """Best-effort removal of every version and the bucket itself."""
    try:
        for page in s3.get_paginator("list_object_versions").paginate(Bucket=bucket):
            for v in page.get("Versions", []) + page.get("DeleteMarkers", []):
                s3.delete_object(
                    Bucket=bucket,
                    Key=v["Key"],
                    VersionId=v["VersionId"],
                    BypassGovernanceRetention=True,
                )
    except ClientError:
        pass
    try:
        s3.delete_bucket(Bucket=bucket)
    except ClientError:
        pass


@pytest.fixture
def make_bucket(s3_client):
    """Creates root-owned buckets, removed at teardown."""
    created = []

    def make(prefix, **kwargs):
        name = f"{prefix}-{uuid.uuid4().hex[:8]}"
        s3_client.create_bucket(Bucket=name, **kwargs)
        created.append(name)
        return name

    yield make
    for name in created:
        _empty_and_delete(s3_client, name)


@pytest.fixture
def victim(s3_client, make_bucket):
    bucket = make_bucket("victim")
    s3_client.put_object(Bucket=bucket, Key="secret.txt", Body=b"top secret")
    return bucket


@pytest.fixture
def own(make_bucket):
    return make_bucket("own")


def _assert_access_denied(call):
    with pytest.raises(ClientError) as exc:
        call()
    assert exc.value.response["Error"]["Code"] == "AccessDenied"


class TestCopySourceAuthorization:
    def test_copy_from_an_unreadable_bucket_is_denied(
        self, endpoint_url, s3_client, victim, own
    ):
        with restricted_user(endpoint_url, own_bucket_statements(own)) as creds:
            user = s3_client_for(endpoint_url, creds)
            _assert_access_denied(
                lambda: user.copy_object(
                    Bucket=own,
                    Key="stolen.txt",
                    CopySource={"Bucket": victim, "Key": "secret.txt"},
                )
            )
        with pytest.raises(ClientError):
            s3_client.head_object(Bucket=own, Key="stolen.txt")

    def test_a_refused_copy_does_not_reveal_what_exists(
        self, endpoint_url, victim, own
    ):
        with restricted_user(endpoint_url, own_bucket_statements(own)) as creds:
            user = s3_client_for(endpoint_url, creds)
            for source in (
                {"Bucket": victim, "Key": "no-such-key"},
                {"Bucket": f"no-such-{uuid.uuid4().hex[:8]}", "Key": "k"},
            ):
                _assert_access_denied(
                    lambda: user.copy_object(Bucket=own, Key="probe", CopySource=source)
                )

    def test_copy_within_the_own_bucket_works(self, endpoint_url, own):
        with restricted_user(endpoint_url, own_bucket_statements(own)) as creds:
            user = s3_client_for(endpoint_url, creds)
            user.put_object(Bucket=own, Key="a.txt", Body=b"mine")
            user.copy_object(Bucket=own, Key="b.txt", CopySource={"Bucket": own, "Key": "a.txt"})
            assert user.get_object(Bucket=own, Key="b.txt")["Body"].read() == b"mine"

    def test_copy_from_a_readable_source_works(self, endpoint_url, victim, own):
        statements = own_bucket_statements(own) + [
            allow(["s3:GetObject"], [f"arn:aws:s3:::{victim}/*"])
        ]
        with restricted_user(endpoint_url, statements) as creds:
            user = s3_client_for(endpoint_url, creds)
            user.copy_object(
                Bucket=own, Key="copy.txt", CopySource={"Bucket": victim, "Key": "secret.txt"}
            )
            assert user.get_object(Bucket=own, Key="copy.txt")["Body"].read() == b"top secret"

    def test_upload_part_copy_from_an_unreadable_bucket_is_denied(
        self, endpoint_url, victim, own
    ):
        with restricted_user(endpoint_url, own_bucket_statements(own)) as creds:
            user = s3_client_for(endpoint_url, creds)
            upload_id = user.create_multipart_upload(Bucket=own, Key="mp")["UploadId"]
            try:
                _assert_access_denied(
                    lambda: user.upload_part_copy(
                        Bucket=own,
                        Key="mp",
                        UploadId=upload_id,
                        PartNumber=1,
                        CopySource={"Bucket": victim, "Key": "secret.txt"},
                    )
                )
            finally:
                user.abort_multipart_upload(Bucket=own, Key="mp", UploadId=upload_id)


class TestArchiveAuthorization:
    def _archive(self, endpoint_url, creds, bucket, keys):
        return signed_request(
            "POST",
            f"{endpoint_url}/admin/archive",
            creds,
            data={"bucket": bucket, "keys": keys},
        )

    def _statements(self, own):
        return own_bucket_statements(own) + [allow(["arca:CreateArchive"], ["*"])]

    def test_archiving_unreadable_objects_is_denied(self, endpoint_url, victim, own):
        with restricted_user(endpoint_url, self._statements(own)) as creds:
            resp = self._archive(endpoint_url, creds, victim, ["secret.txt"])
            assert resp.status_code == 403, resp.text
            assert resp.json()["error"] == "AccessDenied"

    def test_a_refused_archive_does_not_reveal_what_exists(
        self, endpoint_url, victim, own
    ):
        with restricted_user(endpoint_url, self._statements(own)) as creds:
            for bucket, key in (
                (victim, "no-such-key"),
                (f"no-such-{uuid.uuid4().hex[:8]}", "k"),
            ):
                resp = self._archive(endpoint_url, creds, bucket, [key])
                assert resp.status_code == 403, resp.text

    def test_one_unreadable_key_denies_the_whole_archive(
        self, endpoint_url, s3_client, own
    ):
        s3_client.put_object(Bucket=own, Key="public/a", Body=b"a")
        s3_client.put_object(Bucket=own, Key="private/b", Body=b"b")
        statements = [
            allow(["arca:CreateArchive"], ["*"]),
            allow(["s3:GetObject"], [f"arn:aws:s3:::{own}/public/*"]),
        ]
        with restricted_user(endpoint_url, statements) as creds:
            resp = self._archive(endpoint_url, creds, own, ["public/a", "private/b"])
            assert resp.status_code == 403, resp.text

    def test_archiving_readable_objects_works(self, endpoint_url, own):
        with restricted_user(endpoint_url, self._statements(own)) as creds:
            s3_client_for(endpoint_url, creds).put_object(Bucket=own, Key="mine.txt", Body=b"x")
            resp = self._archive(endpoint_url, creds, own, ["mine.txt"])
            assert resp.status_code == 200, resp.text
            assert resp.headers["Content-Type"] == "application/gzip"


class TestDeleteObjectsAuthorization:
    def test_only_the_deletable_keys_are_deleted(self, endpoint_url, s3_client, victim):
        s3_client.put_object(Bucket=victim, Key="public/a", Body=b"a")
        s3_client.put_object(Bucket=victim, Key="private/b", Body=b"b")
        statements = [allow(["s3:DeleteObject"], [f"arn:aws:s3:::{victim}/public/*"])]
        with restricted_user(endpoint_url, statements) as creds:
            user = s3_client_for(endpoint_url, creds)
            resp = user.delete_objects(
                Bucket=victim,
                Delete={"Objects": [{"Key": "public/a"}, {"Key": "private/b"}]},
            )
        assert [d["Key"] for d in resp.get("Deleted", [])] == ["public/a"]
        errors = resp.get("Errors", [])
        assert [(e["Key"], e["Code"]) for e in errors] == [("private/b", "AccessDenied")]
        s3_client.head_object(Bucket=victim, Key="private/b")

    def test_a_grant_on_the_bucket_alone_deletes_no_object(
        self, endpoint_url, s3_client, victim
    ):
        statements = [allow(["s3:*"], [f"arn:aws:s3:::{victim}"])]
        with restricted_user(endpoint_url, statements) as creds:
            user = s3_client_for(endpoint_url, creds)
            _assert_access_denied(
                lambda: user.delete_objects(
                    Bucket=victim, Delete={"Objects": [{"Key": "secret.txt"}]}
                )
            )
        s3_client.head_object(Bucket=victim, Key="secret.txt")

    def test_a_refused_batch_does_not_reveal_a_missing_bucket(self, endpoint_url, own):
        with restricted_user(endpoint_url, own_bucket_statements(own)) as creds:
            user = s3_client_for(endpoint_url, creds)
            _assert_access_denied(
                lambda: user.delete_objects(
                    Bucket=f"no-such-{uuid.uuid4().hex[:8]}",
                    Delete={"Objects": [{"Key": "k"}]},
                )
            )


    def test_an_empty_batch_is_malformed(self, endpoint_url, own):
        """AWS requires 1 to 1000 keys; refusing early also keeps an empty
        batch from probing whether a bucket exists."""
        with restricted_user(endpoint_url, own_bucket_statements(own)) as creds:
            for bucket in (own, f"no-such-{uuid.uuid4().hex[:8]}"):
                resp = signed_request(
                    "POST",
                    f"{endpoint_url}/{bucket}?delete",
                    creds,
                    data="<Delete></Delete>",
                )
                assert resp.status_code == 400, resp.text
                assert "<Code>MalformedXML</Code>" in resp.text


class TestGovernanceBypassAuthorization:
    @pytest.fixture
    def locked(self, s3_client, make_bucket):
        """(bucket, version id) of an object under one year of GOVERNANCE."""
        bucket = make_bucket("locked", ObjectLockEnabledForBucket=True)
        vid = s3_client.put_object(Bucket=bucket, Key="k", Body=b"worm")["VersionId"]
        s3_client.put_object_retention(
            Bucket=bucket,
            Key="k",
            Retention={
                "Mode": "GOVERNANCE",
                "RetainUntilDate": datetime.now(timezone.utc) + timedelta(days=365),
            },
        )
        return bucket, vid

    def _statements(self, bucket, extra=()):
        actions = ["s3:GetObject", "s3:PutObject", "s3:DeleteObject", "s3:ListBucket", *extra]
        return [allow(actions, [f"arn:aws:s3:::{bucket}", f"arn:aws:s3:::{bucket}/*"])]

    def test_bypass_without_the_permission_cannot_delete(
        self, endpoint_url, s3_client, locked
    ):
        bucket, vid = locked
        with restricted_user(endpoint_url, self._statements(bucket)) as creds:
            user = s3_client_for(endpoint_url, creds)
            _assert_access_denied(
                lambda: user.delete_object(
                    Bucket=bucket, Key="k", VersionId=vid, BypassGovernanceRetention=True
                )
            )
        s3_client.head_object(Bucket=bucket, Key="k", VersionId=vid)

    def test_bypass_without_the_permission_cannot_shorten_retention(
        self, endpoint_url, locked
    ):
        bucket, _ = locked
        with restricted_user(endpoint_url, self._statements(bucket)) as creds:
            user = s3_client_for(endpoint_url, creds)
            _assert_access_denied(
                lambda: user.put_object_retention(
                    Bucket=bucket,
                    Key="k",
                    BypassGovernanceRetention=True,
                    Retention={
                        "Mode": "GOVERNANCE",
                        "RetainUntilDate": datetime.now(timezone.utc) + timedelta(days=1),
                    },
                )
            )

    def test_bypass_with_the_permission_deletes(self, endpoint_url, locked):
        bucket, vid = locked
        statements = self._statements(bucket, extra=["s3:BypassGovernanceRetention"])
        with restricted_user(endpoint_url, statements) as creds:
            user = s3_client_for(endpoint_url, creds)
            resp = user.delete_object(
                Bucket=bucket, Key="k", VersionId=vid, BypassGovernanceRetention=True
            )
            assert resp["ResponseMetadata"]["HTTPStatusCode"] == 204


class TestPercentEncodedPaths:
    """A percent-encoded bucket or key must not dodge a Deny statement."""

    @pytest.fixture
    def guarded(self, s3_client, make_bucket):
        bucket = make_bucket("guarded")
        s3_client.put_object(Bucket=bucket, Key="private/s", Body=b"hidden")
        s3_client.put_object(Bucket=bucket, Key="public/p", Body=b"shown")
        return bucket

    def _get(self, endpoint_url, creds, raw_path):
        return signed_request("GET", f"{endpoint_url}{raw_path}", creds, keep_url=True)

    def test_an_encoded_key_does_not_dodge_a_deny(self, endpoint_url, guarded):
        statements = [
            allow(["s3:*"], ["*"]),
            {
                "Effect": "Deny",
                "Action": ["s3:GetObject"],
                "Resource": [f"arn:aws:s3:::{guarded}/private/*"],
            },
        ]
        with restricted_user(endpoint_url, statements) as creds:
            resp = self._get(endpoint_url, creds, f"/{guarded}/privat%65/s")
            assert resp.status_code == 403, resp.text
            assert "<Code>AccessDenied</Code>" in resp.text
            assert b"hidden" not in resp.content
            assert self._get(endpoint_url, creds, f"/{guarded}/public/p").content == b"shown"

    def test_an_encoded_bucket_does_not_dodge_a_deny(self, endpoint_url, guarded):
        encoded = "%" + format(ord(guarded[0]), "02X") + guarded[1:]
        statements = [
            allow(["s3:*"], ["*"]),
            {
                "Effect": "Deny",
                "Action": ["s3:*"],
                "Resource": [f"arn:aws:s3:::{guarded}", f"arn:aws:s3:::{guarded}/*"],
            },
        ]
        with restricted_user(endpoint_url, statements) as creds:
            for path in (f"/{encoded}/public/p", f"/{encoded}?list-type=2"):
                resp = self._get(endpoint_url, creds, path)
                assert resp.status_code == 403, f"{path}: {resp.text}"
                assert "<Code>AccessDenied</Code>" in resp.text, f"{path}: {resp.text}"

    def test_an_encoded_key_matches_a_grant_on_its_decoded_name(
        self, endpoint_url, s3_client, guarded
    ):
        s3_client.put_object(Bucket=guarded, Key="a b.txt", Body=b"spaced")
        statements = [allow(["s3:GetObject"], [f"arn:aws:s3:::{guarded}/a b.txt"])]
        with restricted_user(endpoint_url, statements) as creds:
            resp = self._get(endpoint_url, creds, f"/{guarded}/a%20b.txt")
            assert resp.status_code == 200, resp.text
            assert resp.content == b"spaced"
