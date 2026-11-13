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
    ROOT,
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


def _assert_allowed(call):
    """The call passes authorization, whatever the operation answers next
    (GetBucketEncryption is 404 on a server without encryption)."""
    try:
        call()
    except ClientError as exc:
        assert exc.response["Error"]["Code"] != "AccessDenied", exc


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
        """Everything these operations need except the bypass itself."""
        actions = [
            "s3:GetObject",
            "s3:GetObjectVersion",
            "s3:PutObject",
            "s3:PutObjectRetention",
            "s3:DeleteObject",
            "s3:DeleteObjectVersion",
            "s3:ListBucket",
            *extra,
        ]
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


def _raw(endpoint_url, creds, method, raw_path, data=None):
    return signed_request(method, f"{endpoint_url}{raw_path}", creds, data=data, keep_url=True)


class TestSubresourceClassification:
    """What the auth middleware authorizes is what the handler runs.

    Both read the query through one classifier with exact key matching, so a
    key that merely contains a subresource name (`?xencryption`) is not that
    subresource, and a query naming two of them is the same operation on both
    sides.
    """

    def test_xencryption_cannot_list(self, endpoint_url, victim):
        statements = [allow(["s3:GetEncryptionConfiguration"], ["*"])]
        with restricted_user(endpoint_url, statements) as creds:
            resp = _raw(endpoint_url, creds, "GET", f"/{victim}?xencryption")
            assert resp.status_code == 403, resp.text
            assert b"secret.txt" not in resp.content

    def test_xencryption_cannot_create_a_bucket(self, endpoint_url, s3_client):
        name = f"sneaky-{uuid.uuid4().hex[:8]}"
        statements = [allow(["s3:PutEncryptionConfiguration"], ["*"])]
        with restricted_user(endpoint_url, statements) as creds:
            resp = _raw(endpoint_url, creds, "PUT", f"/{name}?xencryption")
            assert resp.status_code == 403, resp.text
        with pytest.raises(ClientError):
            s3_client.head_bucket(Bucket=name)

    def test_xencryption_cannot_delete_a_bucket(self, endpoint_url, s3_client, make_bucket):
        empty = make_bucket("empty")
        statements = [allow(["s3:PutEncryptionConfiguration"], ["*"])]
        with restricted_user(endpoint_url, statements) as creds:
            resp = _raw(endpoint_url, creds, "DELETE", f"/{empty}?xencryption")
            assert resp.status_code == 403, resp.text
        s3_client.head_bucket(Bucket=empty)

    def test_a_second_subresource_does_not_reach_lifecycle(
        self, endpoint_url, s3_client, victim
    ):
        lifecycle = (
            '<LifecycleConfiguration><Rule><ID>wipe</ID><Filter><Prefix></Prefix></Filter>'
            "<Status>Enabled</Status><Expiration><Days>1</Days></Expiration></Rule>"
            "</LifecycleConfiguration>"
        )
        statements = [allow(["s3:PutEncryptionConfiguration"], ["*"])]
        with restricted_user(endpoint_url, statements) as creds:
            resp = _raw(
                endpoint_url, creds, "PUT", f"/{victim}?lifecycle&encryption", data=lifecycle
            )
            assert resp.status_code != 200, resp.text
        with pytest.raises(ClientError) as exc:
            s3_client.get_bucket_lifecycle_configuration(Bucket=victim)
        assert exc.value.response["Error"]["Code"] == "NoSuchLifecycleConfiguration"

    def test_an_unimplemented_subresource_does_not_delete_the_bucket(
        self, endpoint_url, s3_client, make_bucket
    ):
        empty = make_bucket("cors")
        resp = signed_request("DELETE", f"{endpoint_url}/{empty}?cors", ROOT, keep_url=True)
        assert resp.status_code == 501, resp.text
        s3_client.head_bucket(Bucket=empty)


class TestGranularBucketActions:
    """A bucket subresource needs its own AWS action, not CreateBucket / ListBucket."""

    def test_create_bucket_cannot_reconfigure_another_bucket(self, endpoint_url, victim):
        with restricted_user(endpoint_url, [allow(["s3:CreateBucket"], ["*"])]) as creds:
            user = s3_client_for(endpoint_url, creds)
            _assert_access_denied(
                lambda: user.put_bucket_versioning(
                    Bucket=victim, VersioningConfiguration={"Status": "Enabled"}
                )
            )
            _assert_access_denied(
                lambda: user.put_bucket_lifecycle_configuration(
                    Bucket=victim,
                    LifecycleConfiguration={
                        "Rules": [
                            {
                                "ID": "wipe",
                                "Filter": {"Prefix": ""},
                                "Status": "Enabled",
                                "Expiration": {"Days": 1},
                            }
                        ]
                    },
                )
            )
            _assert_access_denied(
                lambda: user.put_object_lock_configuration(
                    Bucket=victim, ObjectLockConfiguration={"ObjectLockEnabled": "Enabled"}
                )
            )
            _assert_access_denied(
                lambda: user.put_bucket_tagging(
                    Bucket=victim, Tagging={"TagSet": [{"Key": "k", "Value": "v"}]}
                )
            )
            _assert_access_denied(lambda: user.delete_bucket_lifecycle(Bucket=victim))

    def test_list_bucket_cannot_read_configurations(self, endpoint_url, victim):
        with restricted_user(endpoint_url, [allow(["s3:ListBucket"], ["*"])]) as creds:
            user = s3_client_for(endpoint_url, creds)
            assert user.list_objects_v2(Bucket=victim)["KeyCount"] == 1
            _assert_access_denied(lambda: user.get_bucket_versioning(Bucket=victim))
            _assert_access_denied(lambda: user.get_bucket_tagging(Bucket=victim))
            _assert_access_denied(lambda: user.list_object_versions(Bucket=victim))
            _assert_access_denied(lambda: user.list_multipart_uploads(Bucket=victim))

    def test_each_action_allows_its_operation(self, endpoint_url, victim):
        statements = [
            allow(
                [
                    "s3:GetBucketVersioning",
                    "s3:PutBucketTagging",
                    "s3:GetBucketTagging",
                    "s3:ListBucketVersions",
                    "s3:ListBucketMultipartUploads",
                    "s3:GetEncryptionConfiguration",
                ],
                [f"arn:aws:s3:::{victim}"],
            )
        ]
        with restricted_user(endpoint_url, statements) as creds:
            user = s3_client_for(endpoint_url, creds)
            user.get_bucket_versioning(Bucket=victim)
            user.put_bucket_tagging(Bucket=victim, Tagging={"TagSet": [{"Key": "k", "Value": "v"}]})
            assert user.get_bucket_tagging(Bucket=victim)["TagSet"] == [{"Key": "k", "Value": "v"}]
            user.delete_bucket_tagging(Bucket=victim)
            user.list_object_versions(Bucket=victim)
            user.list_multipart_uploads(Bucket=victim)
            _assert_allowed(lambda: user.get_bucket_encryption(Bucket=victim))
            _assert_access_denied(lambda: user.list_objects_v2(Bucket=victim))

    def test_the_built_in_read_only_grant_reads_but_does_not_write(
        self, endpoint_url, s3_client, victim
    ):
        """S3ReadOnlyAccess, as the server stores it after its migration."""
        resp = signed_request("GET", f"{endpoint_url}/admin/grants/grant-s3-read-only", ROOT)
        assert resp.status_code == 200, resp.text
        statements = resp.json()["document"]["Statement"]
        s3_client.put_object_tagging(
            Bucket=victim, Key="secret.txt", Tagging={"TagSet": [{"Key": "a", "Value": "b"}]}
        )
        with restricted_user(endpoint_url, statements) as creds:
            user = s3_client_for(endpoint_url, creds)
            user.list_buckets()
            user.list_objects_v2(Bucket=victim)
            user.list_object_versions(Bucket=victim)
            user.list_multipart_uploads(Bucket=victim)
            user.get_bucket_versioning(Bucket=victim)
            user.get_bucket_location(Bucket=victim)
            _assert_allowed(lambda: user.get_bucket_encryption(Bucket=victim))
            assert user.get_object(Bucket=victim, Key="secret.txt")["Body"].read() == b"top secret"
            assert user.get_object_tagging(Bucket=victim, Key="secret.txt")["TagSet"]
            _assert_access_denied(
                lambda: user.put_object(Bucket=victim, Key="w", Body=b"w")
            )
            _assert_access_denied(
                lambda: user.put_bucket_versioning(
                    Bucket=victim, VersioningConfiguration={"Status": "Enabled"}
                )
            )
            _assert_access_denied(lambda: user.delete_bucket_encryption(Bucket=victim))

    def test_the_old_encryption_action_names_are_rejected(self, endpoint_url):
        resp = signed_request(
            "POST",
            f"{endpoint_url}/admin/grants",
            ROOT,
            data={
                "name": f"old-{uuid.uuid4().hex[:8]}",
                "document": {
                    "Version": "2012-10-17",
                    "Statement": [allow(["s3:GetBucketEncryption"], ["*"])],
                },
            },
        )
        if resp.status_code == 201:
            signed_request(
                "DELETE", f"{endpoint_url}/admin/grants/{resp.json()['grant_id']}", ROOT
            )
        assert resp.status_code == 400, resp.text


class TestGranularObjectActions:
    """Object subresources, versions and write headers need their own actions."""

    @pytest.fixture
    def versioned(self, s3_client, make_bucket):
        """(bucket, first version id, second version id) of key `k`."""
        bucket = make_bucket("versioned", ObjectLockEnabledForBucket=True)
        v1 = s3_client.put_object(Bucket=bucket, Key="k", Body=b"one")["VersionId"]
        v2 = s3_client.put_object(Bucket=bucket, Key="k", Body=b"two")["VersionId"]
        return bucket, v1, v2

    def _object_statements(self, bucket, actions):
        return [allow(actions, [f"arn:aws:s3:::{bucket}", f"arn:aws:s3:::{bucket}/*"])]

    def test_put_object_alone_cannot_lock_or_tag(self, endpoint_url, s3_client, versioned):
        bucket, _, _ = versioned
        statements = self._object_statements(bucket, ["s3:PutObject", "s3:GetObject"])
        until = datetime.now(timezone.utc) + timedelta(days=3650)
        with restricted_user(endpoint_url, statements) as creds:
            user = s3_client_for(endpoint_url, creds)
            _assert_access_denied(
                lambda: user.put_object_retention(
                    Bucket=bucket,
                    Key="k",
                    Retention={"Mode": "COMPLIANCE", "RetainUntilDate": until},
                )
            )
            _assert_access_denied(
                lambda: user.put_object_legal_hold(
                    Bucket=bucket, Key="k", LegalHold={"Status": "ON"}
                )
            )
            _assert_access_denied(
                lambda: user.put_object(
                    Bucket=bucket,
                    Key="locked",
                    Body=b"x",
                    ObjectLockMode="COMPLIANCE",
                    ObjectLockRetainUntilDate=until,
                )
            )
            _assert_access_denied(
                lambda: user.put_object(
                    Bucket=bucket, Key="held", Body=b"x", ObjectLockLegalHoldStatus="ON"
                )
            )
            # The header PutObject actually honours.
            resp = signed_request(
                "PUT",
                f"{endpoint_url}/{bucket}/held",
                creds,
                data="x",
                headers={"x-amz-object-lock-legal-hold-status": "ON"},
            )
            assert resp.status_code == 403, resp.text
            _assert_access_denied(
                lambda: user.put_object(Bucket=bucket, Key="tagged", Body=b"x", Tagging="a=b")
            )
            _assert_access_denied(
                lambda: user.create_multipart_upload(Bucket=bucket, Key="mp", Tagging="a=b")
            )
            _assert_access_denied(
                lambda: user.put_object_tagging(
                    Bucket=bucket, Key="k", Tagging={"TagSet": [{"Key": "a", "Value": "b"}]}
                )
            )
            # The plain write still works.
            user.put_object(Bucket=bucket, Key="plain", Body=b"x")
        for key in ("locked", "held", "tagged"):
            with pytest.raises(ClientError):
                s3_client.head_object(Bucket=bucket, Key=key)

    def test_get_object_cannot_read_tags_or_lock_state(self, endpoint_url, versioned):
        bucket, _, _ = versioned
        statements = self._object_statements(bucket, ["s3:GetObject"])
        with restricted_user(endpoint_url, statements) as creds:
            user = s3_client_for(endpoint_url, creds)
            assert user.get_object(Bucket=bucket, Key="k")["Body"].read() == b"two"
            _assert_access_denied(lambda: user.get_object_tagging(Bucket=bucket, Key="k"))
            _assert_access_denied(lambda: user.get_object_retention(Bucket=bucket, Key="k"))
            _assert_access_denied(lambda: user.get_object_legal_hold(Bucket=bucket, Key="k"))

    def test_a_version_needs_the_version_actions(self, endpoint_url, s3_client, versioned):
        bucket, v1, _ = versioned
        statements = self._object_statements(
            bucket, ["s3:GetObject", "s3:DeleteObject", "s3:PutObject"]
        )
        with restricted_user(endpoint_url, statements) as creds:
            user = s3_client_for(endpoint_url, creds)
            _assert_access_denied(lambda: user.get_object(Bucket=bucket, Key="k", VersionId=v1))
            with pytest.raises(ClientError) as exc:
                user.head_object(Bucket=bucket, Key="k", VersionId=v1)
            assert exc.value.response["ResponseMetadata"]["HTTPStatusCode"] == 403
            _assert_access_denied(
                lambda: user.delete_object(Bucket=bucket, Key="k", VersionId=v1)
            )
            _assert_access_denied(
                lambda: user.copy_object(
                    Bucket=bucket,
                    Key="copy",
                    CopySource={"Bucket": bucket, "Key": "k", "VersionId": v1},
                )
            )
            _assert_access_denied(
                lambda: user.delete_objects(
                    Bucket=bucket, Delete={"Objects": [{"Key": "k", "VersionId": v1}]}
                )
            )
            # The latest version (a delete marker) is still deletable in a batch.
            result = user.delete_objects(
                Bucket=bucket,
                Delete={"Objects": [{"Key": "k"}, {"Key": "k", "VersionId": v1}]},
            )
            assert [e["Code"] for e in result.get("Errors", [])] == ["AccessDenied"]
            assert [d["Key"] for d in result.get("Deleted", [])] == ["k"]
        s3_client.head_object(Bucket=bucket, Key="k", VersionId=v1)

    def test_the_version_actions_allow_versions(self, endpoint_url, s3_client, versioned):
        bucket, v1, _ = versioned
        statements = self._object_statements(
            bucket, ["s3:GetObjectVersion", "s3:DeleteObjectVersion", "s3:PutObject"]
        )
        with restricted_user(endpoint_url, statements) as creds:
            user = s3_client_for(endpoint_url, creds)
            assert user.get_object(Bucket=bucket, Key="k", VersionId=v1)["Body"].read() == b"one"
            user.copy_object(
                Bucket=bucket,
                Key="copy",
                CopySource={"Bucket": bucket, "Key": "k", "VersionId": v1},
            )
            user.delete_object(Bucket=bucket, Key="k", VersionId=v1)
        with pytest.raises(ClientError):
            s3_client.head_object(Bucket=bucket, Key="k", VersionId=v1)

    def test_tagging_and_lock_actions_allow_their_operations(self, endpoint_url, versioned):
        bucket, _, _ = versioned
        statements = self._object_statements(
            bucket,
            [
                "s3:PutObject",
                "s3:PutObjectTagging",
                "s3:GetObjectTagging",
                "s3:DeleteObjectTagging",
                "s3:PutObjectLegalHold",
                "s3:GetObjectLegalHold",
            ],
        )
        with restricted_user(endpoint_url, statements) as creds:
            user = s3_client_for(endpoint_url, creds)
            user.put_object(Bucket=bucket, Key="t", Body=b"x", Tagging="a=b")
            assert user.get_object_tagging(Bucket=bucket, Key="t")["TagSet"] == [
                {"Key": "a", "Value": "b"}
            ]
            user.delete_object_tagging(Bucket=bucket, Key="t")
            user.put_object_legal_hold(Bucket=bucket, Key="t", LegalHold={"Status": "ON"})
            assert user.get_object_legal_hold(Bucket=bucket, Key="t")["LegalHold"]["Status"] == "ON"
            user.put_object_legal_hold(Bucket=bucket, Key="t", LegalHold={"Status": "OFF"})

    def test_multipart_actions(self, endpoint_url, make_bucket):
        bucket = make_bucket("mp")
        statements = self._object_statements(bucket, ["s3:PutObject"])
        with restricted_user(endpoint_url, statements) as creds:
            user = s3_client_for(endpoint_url, creds)
            uid = user.create_multipart_upload(Bucket=bucket, Key="mp")["UploadId"]
            user.upload_part(Bucket=bucket, Key="mp", UploadId=uid, PartNumber=1, Body=b"x")
            _assert_access_denied(
                lambda: user.list_parts(Bucket=bucket, Key="mp", UploadId=uid)
            )
            _assert_access_denied(
                lambda: user.abort_multipart_upload(Bucket=bucket, Key="mp", UploadId=uid)
            )
        statements = self._object_statements(
            bucket, ["s3:ListMultipartUploadParts", "s3:AbortMultipartUpload"]
        )
        with restricted_user(endpoint_url, statements) as creds:
            user = s3_client_for(endpoint_url, creds)
            assert len(user.list_parts(Bucket=bucket, Key="mp", UploadId=uid)["Parts"]) == 1
            user.abort_multipart_upload(Bucket=bucket, Key="mp", UploadId=uid)
