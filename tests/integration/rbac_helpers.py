"""Helpers for integration tests that act as a non-root user.

`restricted_user` creates a grant from the given policy statements, a user
holding it and a credential, yields the credential and removes all three
afterwards. Admin calls are signed with the root credential.
"""

import json
import os
import uuid
from contextlib import contextmanager

import boto3
import requests
from botocore.auth import S3SigV4Auth
from botocore.awsrequest import AWSRequest
from botocore.credentials import Credentials

ROOT = Credentials(
    access_key=os.environ.get("AWS_ACCESS_KEY_ID", "AKIA5B6BSHJA8CIHZSVG"),
    secret_key=os.environ.get(
        "AWS_SECRET_ACCESS_KEY", "hQBDe6WnX9umjbSGCrld7YRUoYfaQUhUcJS/UAgv"
    ),
)


def signed_request(method, url, creds, data=None, keep_url=False):
    """Sends a SigV4-signed request; a dict body is sent as JSON.

    `keep_url` sends the URL exactly as signed: by default `requests` decodes
    escapes of unreserved characters (`%65` becomes `e`), which breaks the
    signature of a deliberately encoded path.
    """
    headers = {}
    if data is not None:
        headers["Content-Type"] = "application/json"
        data = json.dumps(data) if isinstance(data, dict) else data
    aws_req = AWSRequest(method=method, url=url, data=data or "", headers=headers)
    S3SigV4Auth(creds, "s3", "us-east-1").add_auth(aws_req)
    if not keep_url:
        return requests.request(
            method, url, headers=dict(aws_req.headers), data=data, timeout=10
        )
    with requests.Session() as session:
        prepared = session.prepare_request(
            requests.Request(method, url, headers=dict(aws_req.headers), data=data)
        )
        prepared.url = url
        settings = session.merge_environment_settings(url, {}, None, None, None)
        return session.send(prepared, timeout=10, **settings)


def allow(actions, resources):
    """One Allow statement."""
    return {"Effect": "Allow", "Action": actions, "Resource": resources}


def own_bucket_statements(bucket):
    """The minimal console grant: list bucket names, full access to one bucket."""
    return [
        allow(["s3:ListAllMyBuckets"], ["*"]),
        allow(["s3:*"], [f"arn:aws:s3:::{bucket}", f"arn:aws:s3:::{bucket}/*"]),
    ]


@contextmanager
def restricted_user(endpoint, statements):
    """Yields the Credentials of a fresh user holding only `statements`."""
    suffix = uuid.uuid4().hex[:8]
    resp = signed_request(
        "POST",
        f"{endpoint}/admin/grants",
        ROOT,
        data={
            "name": f"restricted-{suffix}",
            "document": {"Version": "2012-10-17", "Statement": statements},
        },
    )
    assert resp.status_code == 201, resp.text
    grant_id = resp.json()["grant_id"]

    resp = signed_request(
        "POST", f"{endpoint}/admin/users", ROOT, data={"username": f"restricted-{suffix}"}
    )
    assert resp.status_code == 201, resp.text
    user_id = resp.json()["user_id"]
    access_key = None
    try:
        resp = signed_request(
            "PUT", f"{endpoint}/admin/users/{user_id}/grants/{grant_id}", ROOT
        )
        assert resp.status_code in (200, 204), resp.text
        resp = signed_request(
            "POST",
            f"{endpoint}/admin/users/{user_id}/credentials",
            ROOT,
            data={"description": "restricted test user"},
        )
        assert resp.status_code == 201, resp.text
        access_key = resp.json()["access_key_id"]
        yield Credentials(access_key, resp.json()["secret_access_key"])
    finally:
        if access_key:
            signed_request("DELETE", f"{endpoint}/admin/credentials/{access_key}", ROOT)
        signed_request("DELETE", f"{endpoint}/admin/users/{user_id}", ROOT)
        signed_request("DELETE", f"{endpoint}/admin/grants/{grant_id}", ROOT)


def s3_client_for(endpoint, creds):
    """boto3 S3 client signing with `creds`."""
    kwargs = dict(
        endpoint_url=endpoint,
        aws_access_key_id=creds.access_key,
        aws_secret_access_key=creds.secret_key,
        region_name="us-east-1",
    )
    ca_bundle = os.environ.get("AWS_CA_BUNDLE")
    if endpoint.startswith("https://") and ca_bundle:
        kwargs["verify"] = ca_bundle
    return boto3.client("s3", **kwargs)
