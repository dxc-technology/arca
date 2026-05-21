"""Integration tests for Phase 16: Access Control (RBAC).

Tests for user, team, grant management via Admin API, and end-to-end
access control scenarios verifying that policy evaluation works.
"""

import json
import os
import uuid

import boto3
import pytest
import requests
from botocore.auth import S3SigV4Auth
from botocore.awsrequest import AWSRequest
from botocore.credentials import Credentials
from botocore.exceptions import ClientError


# -- Fixtures --


@pytest.fixture
def endpoint(endpoint_url):
    """Base URL for admin API endpoints."""
    return endpoint_url


@pytest.fixture
def creds():
    """Root AWS credentials for SigV4 signing."""
    return Credentials(
        access_key=os.environ.get("AWS_ACCESS_KEY_ID", "AKIAIOSFODNN7EXAMPLE"),
        secret_key=os.environ.get(
            "AWS_SECRET_ACCESS_KEY", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
        ),
    )


def signed_request(method, url, creds, data=None):
    """Make an HTTP request signed with SigV4."""
    headers = {}
    if data is not None:
        headers["Content-Type"] = "application/json"
        data = json.dumps(data) if isinstance(data, dict) else data

    aws_req = AWSRequest(method=method, url=url, data=data or "", headers=headers)
    S3SigV4Auth(creds, "s3", "us-east-1").add_auth(aws_req)
    return requests.request(
        method, url, headers=dict(aws_req.headers), data=data, timeout=10
    )


# ====================================================================
# User Management
# ====================================================================


class TestUserCRUD:
    """Tests for /admin/users endpoints."""

    def test_list_users(self, endpoint, creds):
        resp = signed_request("GET", f"{endpoint}/admin/users", creds)
        assert resp.status_code == 200
        body = resp.json()
        assert isinstance(body, list)
        # Root user should always exist.
        root = [u for u in body if u["is_root"]]
        assert len(root) >= 1

    def test_create_user(self, endpoint, creds):
        name = f"test-user-{uuid.uuid4().hex[:8]}"
        resp = signed_request(
            "POST",
            f"{endpoint}/admin/users",
            creds,
            data={"username": name, "description": "integration test user"},
        )
        assert resp.status_code == 201
        body = resp.json()
        assert body["username"] == name
        assert body["description"] == "integration test user"
        assert body["is_root"] is False
        assert "user_id" in body

        # Cleanup
        signed_request("DELETE", f"{endpoint}/admin/users/{body['user_id']}", creds)

    def test_create_user_duplicate_username(self, endpoint, creds):
        name = f"dup-user-{uuid.uuid4().hex[:8]}"
        resp = signed_request(
            "POST", f"{endpoint}/admin/users", creds, data={"username": name}
        )
        assert resp.status_code == 201
        user_id = resp.json()["user_id"]

        try:
            resp = signed_request(
                "POST", f"{endpoint}/admin/users", creds, data={"username": name}
            )
            assert resp.status_code == 409
        finally:
            signed_request("DELETE", f"{endpoint}/admin/users/{user_id}", creds)

    def test_create_user_empty_username(self, endpoint, creds):
        resp = signed_request(
            "POST", f"{endpoint}/admin/users", creds, data={"username": ""}
        )
        assert resp.status_code == 400

    def test_get_user(self, endpoint, creds):
        name = f"get-user-{uuid.uuid4().hex[:8]}"
        resp = signed_request(
            "POST", f"{endpoint}/admin/users", creds, data={"username": name}
        )
        user_id = resp.json()["user_id"]

        try:
            resp = signed_request(
                "GET", f"{endpoint}/admin/users/{user_id}", creds
            )
            assert resp.status_code == 200
            body = resp.json()
            assert body["username"] == name
            assert body["credential_count"] == 0
            assert body["team_count"] == 0
            assert body["grant_count"] == 0
        finally:
            signed_request("DELETE", f"{endpoint}/admin/users/{user_id}", creds)

    def test_get_user_not_found(self, endpoint, creds):
        resp = signed_request(
            "GET", f"{endpoint}/admin/users/nonexistent-id", creds
        )
        assert resp.status_code == 404

    def test_update_user(self, endpoint, creds):
        name = f"upd-user-{uuid.uuid4().hex[:8]}"
        resp = signed_request(
            "POST", f"{endpoint}/admin/users", creds, data={"username": name}
        )
        user_id = resp.json()["user_id"]

        try:
            resp = signed_request(
                "PUT",
                f"{endpoint}/admin/users/{user_id}",
                creds,
                data={"description": "updated desc"},
            )
            assert resp.status_code == 204

            resp = signed_request(
                "GET", f"{endpoint}/admin/users/{user_id}", creds
            )
            assert resp.json()["description"] == "updated desc"
        finally:
            signed_request("DELETE", f"{endpoint}/admin/users/{user_id}", creds)

    def test_cannot_modify_root_user(self, endpoint, creds):
        resp = signed_request("GET", f"{endpoint}/admin/users", creds)
        root = [u for u in resp.json() if u["is_root"]][0]

        resp = signed_request(
            "PUT",
            f"{endpoint}/admin/users/{root['user_id']}",
            creds,
            data={"description": "hacked"},
        )
        assert resp.status_code == 409

    def test_delete_user(self, endpoint, creds):
        name = f"del-user-{uuid.uuid4().hex[:8]}"
        resp = signed_request(
            "POST", f"{endpoint}/admin/users", creds, data={"username": name}
        )
        user_id = resp.json()["user_id"]

        resp = signed_request(
            "DELETE", f"{endpoint}/admin/users/{user_id}", creds
        )
        assert resp.status_code == 204

        # Verify gone
        resp = signed_request(
            "GET", f"{endpoint}/admin/users/{user_id}", creds
        )
        assert resp.status_code == 404

    def test_cannot_delete_root_user(self, endpoint, creds):
        resp = signed_request("GET", f"{endpoint}/admin/users", creds)
        root = [u for u in resp.json() if u["is_root"]][0]

        resp = signed_request(
            "DELETE", f"{endpoint}/admin/users/{root['user_id']}", creds
        )
        assert resp.status_code == 409

    def test_cannot_delete_user_with_credentials(self, endpoint, creds):
        name = f"cred-user-{uuid.uuid4().hex[:8]}"
        resp = signed_request(
            "POST", f"{endpoint}/admin/users", creds, data={"username": name}
        )
        user_id = resp.json()["user_id"]

        # Create a credential for this user
        resp = signed_request(
            "POST",
            f"{endpoint}/admin/users/{user_id}/credentials",
            creds,
            data={"description": "test"},
        )
        assert resp.status_code == 201
        cred_key = resp.json()["access_key_id"]

        try:
            # Try to delete user — should fail
            resp = signed_request(
                "DELETE", f"{endpoint}/admin/users/{user_id}", creds
            )
            assert resp.status_code == 409
            assert "credentials" in resp.json()["message"].lower()
        finally:
            # Cleanup: remove credential, then user
            signed_request(
                "DELETE", f"{endpoint}/admin/credentials/{cred_key}", creds
            )
            signed_request(
                "DELETE", f"{endpoint}/admin/users/{user_id}", creds
            )


# ====================================================================
# User Credentials
# ====================================================================


class TestUserCredentials:
    """Tests for /admin/users/{user_id}/credentials endpoints."""

    def test_list_user_credentials(self, endpoint, creds):
        name = f"ucred-user-{uuid.uuid4().hex[:8]}"
        resp = signed_request(
            "POST", f"{endpoint}/admin/users", creds, data={"username": name}
        )
        user_id = resp.json()["user_id"]

        try:
            resp = signed_request(
                "GET", f"{endpoint}/admin/users/{user_id}/credentials", creds
            )
            assert resp.status_code == 200
            assert resp.json() == []
        finally:
            signed_request("DELETE", f"{endpoint}/admin/users/{user_id}", creds)

    def test_create_user_credential(self, endpoint, creds):
        name = f"ucred2-user-{uuid.uuid4().hex[:8]}"
        resp = signed_request(
            "POST", f"{endpoint}/admin/users", creds, data={"username": name}
        )
        user_id = resp.json()["user_id"]

        try:
            resp = signed_request(
                "POST",
                f"{endpoint}/admin/users/{user_id}/credentials",
                creds,
                data={"description": "user cred"},
            )
            assert resp.status_code == 201
            body = resp.json()
            assert "access_key_id" in body
            assert "secret_access_key" in body
            assert body["user_id"] == user_id

            cred_key = body["access_key_id"]

            # Verify it shows in the user's credential list
            resp = signed_request(
                "GET", f"{endpoint}/admin/users/{user_id}/credentials", creds
            )
            keys = [c["access_key_id"] for c in resp.json()]
            assert cred_key in keys

            # Cleanup
            signed_request(
                "DELETE", f"{endpoint}/admin/credentials/{cred_key}", creds
            )
        finally:
            signed_request("DELETE", f"{endpoint}/admin/users/{user_id}", creds)

    def test_create_user_credential_custom_keys(self, endpoint, creds):
        name = f"ucred3-user-{uuid.uuid4().hex[:8]}"
        resp = signed_request(
            "POST", f"{endpoint}/admin/users", creds, data={"username": name}
        )
        user_id = resp.json()["user_id"]
        custom_ak = f"CUSTOM{uuid.uuid4().hex[:16].upper()}"
        custom_sk = f"custom-secret-{uuid.uuid4().hex}"

        try:
            resp = signed_request(
                "POST",
                f"{endpoint}/admin/users/{user_id}/credentials",
                creds,
                data={
                    "description": "custom keys",
                    "access_key_id": custom_ak,
                    "secret_access_key": custom_sk,
                },
            )
            assert resp.status_code == 201
            body = resp.json()
            assert body["access_key_id"] == custom_ak
            assert body["secret_access_key"] == custom_sk

            # Cleanup
            signed_request(
                "DELETE", f"{endpoint}/admin/credentials/{custom_ak}", creds
            )
        finally:
            signed_request("DELETE", f"{endpoint}/admin/users/{user_id}", creds)


# ====================================================================
# Team Management
# ====================================================================


class TestTeamCRUD:
    """Tests for /admin/teams endpoints."""

    def test_list_teams(self, endpoint, creds):
        resp = signed_request("GET", f"{endpoint}/admin/teams", creds)
        assert resp.status_code == 200
        assert isinstance(resp.json(), list)

    def test_create_team(self, endpoint, creds):
        name = f"test-team-{uuid.uuid4().hex[:8]}"
        resp = signed_request(
            "POST",
            f"{endpoint}/admin/teams",
            creds,
            data={"name": name, "description": "test team"},
        )
        assert resp.status_code == 201
        body = resp.json()
        assert body["name"] == name
        assert "team_id" in body

        # Cleanup
        signed_request("DELETE", f"{endpoint}/admin/teams/{body['team_id']}", creds)

    def test_create_team_duplicate_name(self, endpoint, creds):
        name = f"dup-team-{uuid.uuid4().hex[:8]}"
        resp = signed_request(
            "POST", f"{endpoint}/admin/teams", creds, data={"name": name}
        )
        assert resp.status_code == 201
        team_id = resp.json()["team_id"]

        try:
            resp = signed_request(
                "POST", f"{endpoint}/admin/teams", creds, data={"name": name}
            )
            assert resp.status_code == 409
        finally:
            signed_request("DELETE", f"{endpoint}/admin/teams/{team_id}", creds)

    def test_get_team(self, endpoint, creds):
        name = f"get-team-{uuid.uuid4().hex[:8]}"
        resp = signed_request(
            "POST", f"{endpoint}/admin/teams", creds, data={"name": name}
        )
        team_id = resp.json()["team_id"]

        try:
            resp = signed_request(
                "GET", f"{endpoint}/admin/teams/{team_id}", creds
            )
            assert resp.status_code == 200
            body = resp.json()
            assert body["name"] == name
            assert body["member_count"] == 0
            assert body["grant_count"] == 0
        finally:
            signed_request("DELETE", f"{endpoint}/admin/teams/{team_id}", creds)

    def test_update_team(self, endpoint, creds):
        name = f"upd-team-{uuid.uuid4().hex[:8]}"
        resp = signed_request(
            "POST", f"{endpoint}/admin/teams", creds, data={"name": name}
        )
        team_id = resp.json()["team_id"]

        try:
            resp = signed_request(
                "PUT",
                f"{endpoint}/admin/teams/{team_id}",
                creds,
                data={"description": "updated team desc"},
            )
            assert resp.status_code == 204
        finally:
            signed_request("DELETE", f"{endpoint}/admin/teams/{team_id}", creds)

    def test_delete_team(self, endpoint, creds):
        name = f"del-team-{uuid.uuid4().hex[:8]}"
        resp = signed_request(
            "POST", f"{endpoint}/admin/teams", creds, data={"name": name}
        )
        team_id = resp.json()["team_id"]

        resp = signed_request(
            "DELETE", f"{endpoint}/admin/teams/{team_id}", creds
        )
        assert resp.status_code == 204

        # Verify gone
        resp = signed_request(
            "GET", f"{endpoint}/admin/teams/{team_id}", creds
        )
        assert resp.status_code == 404


# ====================================================================
# Team Members
# ====================================================================


class TestTeamMembers:
    """Tests for /admin/teams/{team_id}/members endpoints."""

    def test_add_and_remove_member(self, endpoint, creds):
        # Create user and team
        uname = f"member-user-{uuid.uuid4().hex[:8]}"
        resp = signed_request(
            "POST", f"{endpoint}/admin/users", creds, data={"username": uname}
        )
        user_id = resp.json()["user_id"]

        tname = f"member-team-{uuid.uuid4().hex[:8]}"
        resp = signed_request(
            "POST", f"{endpoint}/admin/teams", creds, data={"name": tname}
        )
        team_id = resp.json()["team_id"]

        try:
            # Add member
            resp = signed_request(
                "PUT",
                f"{endpoint}/admin/teams/{team_id}/members/{user_id}",
                creds,
            )
            assert resp.status_code == 204

            # Verify membership
            resp = signed_request(
                "GET", f"{endpoint}/admin/teams/{team_id}/members", creds
            )
            assert resp.status_code == 200
            members = resp.json()
            assert any(m["user_id"] == user_id for m in members)

            # Verify user sees the team
            resp = signed_request(
                "GET", f"{endpoint}/admin/users/{user_id}/teams", creds
            )
            assert any(t["team_id"] == team_id for t in resp.json())

            # Remove member
            resp = signed_request(
                "DELETE",
                f"{endpoint}/admin/teams/{team_id}/members/{user_id}",
                creds,
            )
            assert resp.status_code == 204

            # Verify removed
            resp = signed_request(
                "GET", f"{endpoint}/admin/teams/{team_id}/members", creds
            )
            assert not any(m["user_id"] == user_id for m in resp.json())
        finally:
            signed_request("DELETE", f"{endpoint}/admin/teams/{team_id}", creds)
            signed_request("DELETE", f"{endpoint}/admin/users/{user_id}", creds)

    def test_add_member_nonexistent_user(self, endpoint, creds):
        tname = f"member-team2-{uuid.uuid4().hex[:8]}"
        resp = signed_request(
            "POST", f"{endpoint}/admin/teams", creds, data={"name": tname}
        )
        team_id = resp.json()["team_id"]

        try:
            resp = signed_request(
                "PUT",
                f"{endpoint}/admin/teams/{team_id}/members/nonexistent",
                creds,
            )
            assert resp.status_code == 404
        finally:
            signed_request("DELETE", f"{endpoint}/admin/teams/{team_id}", creds)


# ====================================================================
# Grant Management
# ====================================================================


class TestGrantCRUD:
    """Tests for /admin/grants endpoints."""

    def test_list_grants(self, endpoint, creds):
        resp = signed_request("GET", f"{endpoint}/admin/grants", creds)
        assert resp.status_code == 200
        body = resp.json()
        assert isinstance(body, list)
        # Built-in grants should exist.
        names = [g["name"] for g in body]
        assert "AdministratorAccess" in names
        assert "S3FullAccess" in names
        assert "S3ReadOnlyAccess" in names

    def test_create_grant(self, endpoint, creds):
        name = f"test-grant-{uuid.uuid4().hex[:8]}"
        document = {
            "Version": "2012-10-17",
            "Statement": [
                {
                    "Effect": "Allow",
                    "Action": ["s3:GetObject"],
                    "Resource": ["arn:aws:s3:::my-bucket/*"],
                }
            ],
        }
        resp = signed_request(
            "POST",
            f"{endpoint}/admin/grants",
            creds,
            data={
                "name": name,
                "description": "test grant",
                "document": document,
            },
        )
        assert resp.status_code == 201
        body = resp.json()
        assert body["name"] == name
        assert "grant_id" in body

        # Cleanup
        signed_request("DELETE", f"{endpoint}/admin/grants/{body['grant_id']}", creds)

    def test_create_grant_invalid_document(self, endpoint, creds):
        name = f"bad-grant-{uuid.uuid4().hex[:8]}"
        resp = signed_request(
            "POST",
            f"{endpoint}/admin/grants",
            creds,
            data={
                "name": name,
                "document": {"invalid": "structure"},
            },
        )
        assert resp.status_code == 400

    def test_create_grant_duplicate_name(self, endpoint, creds):
        name = f"dup-grant-{uuid.uuid4().hex[:8]}"
        document = {
            "Version": "2012-10-17",
            "Statement": [
                {
                    "Effect": "Allow",
                    "Action": ["s3:*"],
                    "Resource": ["*"],
                }
            ],
        }
        resp = signed_request(
            "POST",
            f"{endpoint}/admin/grants",
            creds,
            data={"name": name, "document": document},
        )
        assert resp.status_code == 201
        grant_id = resp.json()["grant_id"]

        try:
            resp = signed_request(
                "POST",
                f"{endpoint}/admin/grants",
                creds,
                data={"name": name, "document": document},
            )
            assert resp.status_code == 409
        finally:
            signed_request(
                "DELETE", f"{endpoint}/admin/grants/{grant_id}", creds
            )

    def test_get_grant(self, endpoint, creds):
        resp = signed_request("GET", f"{endpoint}/admin/grants", creds)
        grants = resp.json()
        admin_grant = [g for g in grants if g["name"] == "AdministratorAccess"][0]

        resp = signed_request(
            "GET", f"{endpoint}/admin/grants/{admin_grant['grant_id']}", creds
        )
        assert resp.status_code == 200
        body = resp.json()
        assert body["name"] == "AdministratorAccess"
        assert "document" in body

    def test_delete_builtin_grant(self, endpoint, creds):
        """Built-in grants should be deletable (they're not special)."""
        # Create a custom grant and delete it
        name = f"del-grant-{uuid.uuid4().hex[:8]}"
        document = {
            "Version": "2012-10-17",
            "Statement": [
                {"Effect": "Allow", "Action": ["s3:*"], "Resource": ["*"]}
            ],
        }
        resp = signed_request(
            "POST",
            f"{endpoint}/admin/grants",
            creds,
            data={"name": name, "document": document},
        )
        grant_id = resp.json()["grant_id"]

        resp = signed_request(
            "DELETE", f"{endpoint}/admin/grants/{grant_id}", creds
        )
        assert resp.status_code == 204


# ====================================================================
# Grant Attachments (User + Team)
# ====================================================================


class TestGrantAttachments:
    """Tests for attaching/detaching grants to users and teams."""

    def test_attach_grant_to_user(self, endpoint, creds):
        # Create user
        uname = f"grant-user-{uuid.uuid4().hex[:8]}"
        resp = signed_request(
            "POST", f"{endpoint}/admin/users", creds, data={"username": uname}
        )
        user_id = resp.json()["user_id"]

        # Get S3ReadOnlyAccess grant
        resp = signed_request("GET", f"{endpoint}/admin/grants", creds)
        readonly = [g for g in resp.json() if g["name"] == "S3ReadOnlyAccess"][0]
        grant_id = readonly["grant_id"]

        try:
            # Attach
            resp = signed_request(
                "PUT",
                f"{endpoint}/admin/users/{user_id}/grants/{grant_id}",
                creds,
            )
            assert resp.status_code == 204

            # Verify
            resp = signed_request(
                "GET", f"{endpoint}/admin/users/{user_id}/grants", creds
            )
            grants = resp.json()
            assert any(g["grant_id"] == grant_id for g in grants)

            # Detach
            resp = signed_request(
                "DELETE",
                f"{endpoint}/admin/users/{user_id}/grants/{grant_id}",
                creds,
            )
            assert resp.status_code == 204

            # Verify detached
            resp = signed_request(
                "GET", f"{endpoint}/admin/users/{user_id}/grants", creds
            )
            assert not any(g["grant_id"] == grant_id for g in resp.json())
        finally:
            signed_request("DELETE", f"{endpoint}/admin/users/{user_id}", creds)

    def test_attach_grant_to_team(self, endpoint, creds):
        # Create team
        tname = f"grant-team-{uuid.uuid4().hex[:8]}"
        resp = signed_request(
            "POST", f"{endpoint}/admin/teams", creds, data={"name": tname}
        )
        team_id = resp.json()["team_id"]

        resp = signed_request("GET", f"{endpoint}/admin/grants", creds)
        s3full = [g for g in resp.json() if g["name"] == "S3FullAccess"][0]
        grant_id = s3full["grant_id"]

        try:
            # Attach
            resp = signed_request(
                "PUT",
                f"{endpoint}/admin/teams/{team_id}/grants/{grant_id}",
                creds,
            )
            assert resp.status_code == 204

            # Verify
            resp = signed_request(
                "GET", f"{endpoint}/admin/teams/{team_id}/grants", creds
            )
            assert any(g["grant_id"] == grant_id for g in resp.json())

            # Detach
            resp = signed_request(
                "DELETE",
                f"{endpoint}/admin/teams/{team_id}/grants/{grant_id}",
                creds,
            )
            assert resp.status_code == 204
        finally:
            signed_request("DELETE", f"{endpoint}/admin/teams/{team_id}", creds)

    def test_effective_grants_direct(self, endpoint, creds):
        """Effective grants should include directly attached grants."""
        uname = f"eff-user-{uuid.uuid4().hex[:8]}"
        resp = signed_request(
            "POST", f"{endpoint}/admin/users", creds, data={"username": uname}
        )
        user_id = resp.json()["user_id"]

        resp = signed_request("GET", f"{endpoint}/admin/grants", creds)
        readonly = [g for g in resp.json() if g["name"] == "S3ReadOnlyAccess"][0]

        try:
            signed_request(
                "PUT",
                f"{endpoint}/admin/users/{user_id}/grants/{readonly['grant_id']}",
                creds,
            )

            resp = signed_request(
                "GET",
                f"{endpoint}/admin/users/{user_id}/effective-grants",
                creds,
            )
            assert resp.status_code == 200
            effective = resp.json()
            assert any(
                g["grant_id"] == readonly["grant_id"] and g["source"] == "direct"
                for g in effective
            )
        finally:
            signed_request(
                "DELETE",
                f"{endpoint}/admin/users/{user_id}/grants/{readonly['grant_id']}",
                creds,
            )
            signed_request("DELETE", f"{endpoint}/admin/users/{user_id}", creds)

    def test_effective_grants_via_team(self, endpoint, creds):
        """Effective grants should include grants inherited from teams."""
        uname = f"team-eff-user-{uuid.uuid4().hex[:8]}"
        resp = signed_request(
            "POST", f"{endpoint}/admin/users", creds, data={"username": uname}
        )
        user_id = resp.json()["user_id"]

        tname = f"team-eff-team-{uuid.uuid4().hex[:8]}"
        resp = signed_request(
            "POST", f"{endpoint}/admin/teams", creds, data={"name": tname}
        )
        team_id = resp.json()["team_id"]

        resp = signed_request("GET", f"{endpoint}/admin/grants", creds)
        s3full = [g for g in resp.json() if g["name"] == "S3FullAccess"][0]

        try:
            # Add user to team
            signed_request(
                "PUT",
                f"{endpoint}/admin/teams/{team_id}/members/{user_id}",
                creds,
            )
            # Attach grant to team
            signed_request(
                "PUT",
                f"{endpoint}/admin/teams/{team_id}/grants/{s3full['grant_id']}",
                creds,
            )

            # Check effective grants
            resp = signed_request(
                "GET",
                f"{endpoint}/admin/users/{user_id}/effective-grants",
                creds,
            )
            effective = resp.json()
            team_source = [
                g
                for g in effective
                if g["grant_id"] == s3full["grant_id"]
                and g["source"].startswith("team:")
            ]
            assert len(team_source) == 1
        finally:
            signed_request(
                "DELETE", f"{endpoint}/admin/teams/{team_id}", creds
            )
            signed_request("DELETE", f"{endpoint}/admin/users/{user_id}", creds)


# ====================================================================
# /admin/me Endpoint
# ====================================================================


class TestAdminMe:
    """Tests for /admin/me identity endpoint."""

    def test_me_returns_root_identity(self, endpoint, creds):
        resp = signed_request("GET", f"{endpoint}/admin/me", creds)
        assert resp.status_code == 200
        body = resp.json()
        assert body["user"]["is_root"] is True
        assert "*" in body["effective_actions"]

    def test_me_returns_non_root_identity(self, endpoint, creds):
        """Non-root user with admin grant should see their own identity."""
        # Create user, grant admin access, create credential
        uname = f"me-user-{uuid.uuid4().hex[:8]}"
        resp = signed_request(
            "POST", f"{endpoint}/admin/users", creds, data={"username": uname}
        )
        user_id = resp.json()["user_id"]

        resp = signed_request("GET", f"{endpoint}/admin/grants", creds)
        admin_grant = [
            g for g in resp.json() if g["name"] == "AdministratorAccess"
        ][0]

        try:
            # Attach admin grant
            signed_request(
                "PUT",
                f"{endpoint}/admin/users/{user_id}/grants/{admin_grant['grant_id']}",
                creds,
            )

            # Create credential for user
            resp = signed_request(
                "POST",
                f"{endpoint}/admin/users/{user_id}/credentials",
                creds,
                data={"description": "me-test"},
            )
            user_cred = resp.json()
            user_creds = Credentials(
                access_key=user_cred["access_key_id"],
                secret_key=user_cred["secret_access_key"],
            )

            # Call /admin/me with user's credentials
            resp = signed_request("GET", f"{endpoint}/admin/me", user_creds)
            assert resp.status_code == 200
            body = resp.json()
            assert body["user"]["username"] == uname
            assert body["user"]["is_root"] is False
            assert len(body["effective_actions"]) > 0

            # Cleanup credential
            signed_request(
                "DELETE",
                f"{endpoint}/admin/credentials/{user_cred['access_key_id']}",
                creds,
            )
        finally:
            signed_request(
                "DELETE",
                f"{endpoint}/admin/users/{user_id}/grants/{admin_grant['grant_id']}",
                creds,
            )
            signed_request("DELETE", f"{endpoint}/admin/users/{user_id}", creds)

    def test_me_works_without_any_grant(self, endpoint, creds):
        """A user with no grants attached must still be able to call /admin/me
        — the endpoint is identity-only and any valid SigV4 credential passes.
        Other admin endpoints (e.g. /admin/info) must remain 403 for the same
        user, proving that the grant gate has not been removed wholesale.
        """
        uname = f"plain-{uuid.uuid4().hex[:8]}"
        resp = signed_request(
            "POST", f"{endpoint}/admin/users", creds, data={"username": uname}
        )
        user_id = resp.json()["user_id"]
        try:
            # Create a credential for the user without attaching any grant.
            resp = signed_request(
                "POST",
                f"{endpoint}/admin/users/{user_id}/credentials",
                creds,
                data={"description": "me-no-grant"},
            )
            user_cred = resp.json()
            user_creds = Credentials(
                access_key=user_cred["access_key_id"],
                secret_key=user_cred["secret_access_key"],
            )

            # /admin/me must succeed and return the user's own username.
            me_resp = signed_request("GET", f"{endpoint}/admin/me", user_creds)
            assert me_resp.status_code == 200, me_resp.text
            body = me_resp.json()
            assert body["user"]["username"] == uname
            assert body["user"]["is_root"] is False
            # No grant attached -> no effective_actions returned.
            assert body["effective_actions"] == []

            # Sanity: /admin/info still 403 for the same user.
            info_resp = signed_request("GET", f"{endpoint}/admin/info", user_creds)
            assert info_resp.status_code == 403, info_resp.text

            signed_request(
                "DELETE",
                f"{endpoint}/admin/credentials/{user_cred['access_key_id']}",
                creds,
            )
        finally:
            signed_request("DELETE", f"{endpoint}/admin/users/{user_id}", creds)


# ====================================================================
# E2E Access Control
# ====================================================================


class TestAccessControlE2E:
    """End-to-end tests verifying policy evaluation on S3 operations."""

    def _setup_user_with_grants(self, endpoint, creds, username, grant_ids):
        """Helper: create user, attach grants, create credential, return (user_id, cred)."""
        resp = signed_request(
            "POST",
            f"{endpoint}/admin/users",
            creds,
            data={"username": username},
        )
        assert resp.status_code == 201
        user_id = resp.json()["user_id"]

        for gid in grant_ids:
            signed_request(
                "PUT",
                f"{endpoint}/admin/users/{user_id}/grants/{gid}",
                creds,
            )

        resp = signed_request(
            "POST",
            f"{endpoint}/admin/users/{user_id}/credentials",
            creds,
            data={"description": f"cred for {username}"},
        )
        assert resp.status_code == 201
        user_cred = resp.json()
        return user_id, user_cred

    def _cleanup_user(self, endpoint, creds, user_id, access_key_id):
        """Helper: remove credential and user."""
        signed_request(
            "DELETE", f"{endpoint}/admin/credentials/{access_key_id}", creds
        )
        signed_request("DELETE", f"{endpoint}/admin/users/{user_id}", creds)

    def _get_grant_id(self, endpoint, creds, name):
        resp = signed_request("GET", f"{endpoint}/admin/grants", creds)
        matches = [g for g in resp.json() if g["name"] == name]
        return matches[0]["grant_id"] if matches else None

    def test_s3_full_access_user_can_crud(self, endpoint, creds):
        """User with S3FullAccess can create buckets, put/get/delete objects."""
        grant_id = self._get_grant_id(endpoint, creds, "S3FullAccess")
        uname = f"s3full-{uuid.uuid4().hex[:8]}"
        user_id, user_cred = self._setup_user_with_grants(
            endpoint, creds, uname, [grant_id]
        )

        try:
            user_s3 = boto3.client(
                "s3",
                endpoint_url=endpoint,
                aws_access_key_id=user_cred["access_key_id"],
                aws_secret_access_key=user_cred["secret_access_key"],
                region_name="us-east-1",
            )
            bucket = f"s3full-test-{uuid.uuid4().hex[:8]}"
            user_s3.create_bucket(Bucket=bucket)
            user_s3.put_object(Bucket=bucket, Key="test.txt", Body=b"hello")
            obj = user_s3.get_object(Bucket=bucket, Key="test.txt")
            assert obj["Body"].read() == b"hello"
            user_s3.delete_object(Bucket=bucket, Key="test.txt")
            user_s3.delete_bucket(Bucket=bucket)
        finally:
            self._cleanup_user(endpoint, creds, user_id, user_cred["access_key_id"])

    def test_readonly_user_can_read_not_write(self, endpoint, creds):
        """User with S3ReadOnlyAccess can read but not create/delete."""
        grant_id = self._get_grant_id(endpoint, creds, "S3ReadOnlyAccess")
        uname = f"s3ro-{uuid.uuid4().hex[:8]}"
        user_id, user_cred = self._setup_user_with_grants(
            endpoint, creds, uname, [grant_id]
        )

        bucket = f"ro-test-{uuid.uuid4().hex[:8]}"
        # Root creates a bucket and object first
        s3_root = boto3.client(
            "s3",
            endpoint_url=endpoint,
            aws_access_key_id=os.environ.get(
                "AWS_ACCESS_KEY_ID", "AKIAIOSFODNN7EXAMPLE"
            ),
            aws_secret_access_key=os.environ.get(
                "AWS_SECRET_ACCESS_KEY", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
            ),
            region_name="us-east-1",
        )

        try:
            s3_root.create_bucket(Bucket=bucket)
            s3_root.put_object(Bucket=bucket, Key="data.txt", Body=b"content")

            user_s3 = boto3.client(
                "s3",
                endpoint_url=endpoint,
                aws_access_key_id=user_cred["access_key_id"],
                aws_secret_access_key=user_cred["secret_access_key"],
                region_name="us-east-1",
            )

            # Read should succeed
            obj = user_s3.get_object(Bucket=bucket, Key="data.txt")
            assert obj["Body"].read() == b"content"

            # List should succeed
            resp = user_s3.list_objects_v2(Bucket=bucket)
            assert resp["KeyCount"] == 1

            # Head should succeed
            head = user_s3.head_object(Bucket=bucket, Key="data.txt")
            assert head["ContentLength"] == 7

            # Write should be denied
            with pytest.raises(ClientError) as exc:
                user_s3.put_object(
                    Bucket=bucket, Key="denied.txt", Body=b"nope"
                )
            assert exc.value.response["Error"]["Code"] == "AccessDenied"

            # Delete should be denied
            with pytest.raises(ClientError) as exc:
                user_s3.delete_object(Bucket=bucket, Key="data.txt")
            assert exc.value.response["Error"]["Code"] == "AccessDenied"

            # CreateBucket should be denied
            with pytest.raises(ClientError) as exc:
                user_s3.create_bucket(Bucket="denied-bucket")
            assert exc.value.response["Error"]["Code"] == "AccessDenied"
        finally:
            # Root cleans up
            try:
                s3_root.delete_object(Bucket=bucket, Key="data.txt")
                s3_root.delete_bucket(Bucket=bucket)
            except Exception:
                pass
            self._cleanup_user(endpoint, creds, user_id, user_cred["access_key_id"])

    def test_user_with_no_grants_denied(self, endpoint, creds):
        """User with no grants should be denied for all S3 operations."""
        uname = f"nogrant-{uuid.uuid4().hex[:8]}"
        user_id, user_cred = self._setup_user_with_grants(
            endpoint, creds, uname, []
        )

        try:
            user_s3 = boto3.client(
                "s3",
                endpoint_url=endpoint,
                aws_access_key_id=user_cred["access_key_id"],
                aws_secret_access_key=user_cred["secret_access_key"],
                region_name="us-east-1",
            )

            with pytest.raises(ClientError) as exc:
                user_s3.list_buckets()
            assert exc.value.response["Error"]["Code"] == "AccessDenied"

            with pytest.raises(ClientError) as exc:
                user_s3.create_bucket(Bucket="denied-bucket")
            assert exc.value.response["Error"]["Code"] == "AccessDenied"
        finally:
            self._cleanup_user(endpoint, creds, user_id, user_cred["access_key_id"])

    def test_admin_user_can_access_admin_api(self, endpoint, creds):
        """User with AdministratorAccess grant can call admin endpoints."""
        grant_id = self._get_grant_id(endpoint, creds, "AdministratorAccess")
        uname = f"admin-e2e-{uuid.uuid4().hex[:8]}"
        user_id, user_cred = self._setup_user_with_grants(
            endpoint, creds, uname, [grant_id]
        )

        try:
            user_creds = Credentials(
                access_key=user_cred["access_key_id"],
                secret_key=user_cred["secret_access_key"],
            )

            # Should be able to access admin endpoints
            resp = signed_request("GET", f"{endpoint}/admin/info", user_creds)
            assert resp.status_code == 200

            resp = signed_request("GET", f"{endpoint}/admin/stats", user_creds)
            assert resp.status_code == 200

            resp = signed_request("GET", f"{endpoint}/admin/users", user_creds)
            assert resp.status_code == 200
        finally:
            self._cleanup_user(endpoint, creds, user_id, user_cred["access_key_id"])

    def test_non_admin_user_denied_admin_api(self, endpoint, creds):
        """User with S3FullAccess but no admin grant is denied admin endpoints."""
        grant_id = self._get_grant_id(endpoint, creds, "S3FullAccess")
        uname = f"noadmin-{uuid.uuid4().hex[:8]}"
        user_id, user_cred = self._setup_user_with_grants(
            endpoint, creds, uname, [grant_id]
        )

        try:
            user_creds = Credentials(
                access_key=user_cred["access_key_id"],
                secret_key=user_cred["secret_access_key"],
            )

            resp = signed_request("GET", f"{endpoint}/admin/info", user_creds)
            assert resp.status_code == 403

            resp = signed_request("GET", f"{endpoint}/admin/users", user_creds)
            assert resp.status_code == 403
        finally:
            self._cleanup_user(endpoint, creds, user_id, user_cred["access_key_id"])

    def test_team_inherited_grants_work(self, endpoint, creds):
        """Grants inherited via team membership should authorize S3 ops."""
        s3full_id = self._get_grant_id(endpoint, creds, "S3FullAccess")

        uname = f"team-s3-{uuid.uuid4().hex[:8]}"
        resp = signed_request(
            "POST", f"{endpoint}/admin/users", creds, data={"username": uname}
        )
        user_id = resp.json()["user_id"]

        tname = f"team-s3-{uuid.uuid4().hex[:8]}"
        resp = signed_request(
            "POST", f"{endpoint}/admin/teams", creds, data={"name": tname}
        )
        team_id = resp.json()["team_id"]

        # Create credential for user (no direct grants)
        resp = signed_request(
            "POST",
            f"{endpoint}/admin/users/{user_id}/credentials",
            creds,
            data={"description": "team test"},
        )
        user_cred = resp.json()

        try:
            # Add user to team and attach grant to team
            signed_request(
                "PUT",
                f"{endpoint}/admin/teams/{team_id}/members/{user_id}",
                creds,
            )
            signed_request(
                "PUT",
                f"{endpoint}/admin/teams/{team_id}/grants/{s3full_id}",
                creds,
            )

            user_s3 = boto3.client(
                "s3",
                endpoint_url=endpoint,
                aws_access_key_id=user_cred["access_key_id"],
                aws_secret_access_key=user_cred["secret_access_key"],
                region_name="us-east-1",
            )

            # Should be able to do S3 ops via team grant
            bucket = f"team-s3-test-{uuid.uuid4().hex[:8]}"
            user_s3.create_bucket(Bucket=bucket)
            user_s3.put_object(Bucket=bucket, Key="hello.txt", Body=b"world")
            obj = user_s3.get_object(Bucket=bucket, Key="hello.txt")
            assert obj["Body"].read() == b"world"
            user_s3.delete_object(Bucket=bucket, Key="hello.txt")
            user_s3.delete_bucket(Bucket=bucket)
        finally:
            signed_request(
                "DELETE",
                f"{endpoint}/admin/credentials/{user_cred['access_key_id']}",
                creds,
            )
            signed_request("DELETE", f"{endpoint}/admin/teams/{team_id}", creds)
            signed_request("DELETE", f"{endpoint}/admin/users/{user_id}", creds)

    def test_custom_policy_restricts_to_bucket(self, endpoint, creds):
        """Custom grant limiting to a specific bucket should work."""
        bucket = f"restricted-{uuid.uuid4().hex[:8]}"
        other_bucket = f"forbidden-{uuid.uuid4().hex[:8]}"

        # Create a custom grant
        document = {
            "Version": "2012-10-17",
            "Statement": [
                {
                    "Effect": "Allow",
                    "Action": ["s3:*"],
                    "Resource": [
                        f"arn:aws:s3:::{bucket}",
                        f"arn:aws:s3:::{bucket}/*",
                    ],
                },
                {
                    "Effect": "Allow",
                    "Action": ["s3:ListAllMyBuckets"],
                    "Resource": ["*"],
                },
            ],
        }
        gname = f"custom-{uuid.uuid4().hex[:8]}"
        resp = signed_request(
            "POST",
            f"{endpoint}/admin/grants",
            creds,
            data={"name": gname, "document": document},
        )
        assert resp.status_code == 201
        grant_id = resp.json()["grant_id"]

        uname = f"restricted-{uuid.uuid4().hex[:8]}"
        user_id, user_cred = self._setup_user_with_grants(
            endpoint, creds, uname, [grant_id]
        )

        # Root creates both buckets
        s3_root = boto3.client(
            "s3",
            endpoint_url=endpoint,
            aws_access_key_id=os.environ.get(
                "AWS_ACCESS_KEY_ID", "AKIAIOSFODNN7EXAMPLE"
            ),
            aws_secret_access_key=os.environ.get(
                "AWS_SECRET_ACCESS_KEY", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
            ),
            region_name="us-east-1",
        )

        try:
            s3_root.create_bucket(Bucket=bucket)
            s3_root.create_bucket(Bucket=other_bucket)

            user_s3 = boto3.client(
                "s3",
                endpoint_url=endpoint,
                aws_access_key_id=user_cred["access_key_id"],
                aws_secret_access_key=user_cred["secret_access_key"],
                region_name="us-east-1",
            )

            # Allowed bucket — should work
            user_s3.put_object(Bucket=bucket, Key="ok.txt", Body=b"yes")
            obj = user_s3.get_object(Bucket=bucket, Key="ok.txt")
            assert obj["Body"].read() == b"yes"

            # Forbidden bucket — should be denied
            with pytest.raises(ClientError) as exc:
                user_s3.put_object(
                    Bucket=other_bucket, Key="nope.txt", Body=b"no"
                )
            assert exc.value.response["Error"]["Code"] == "AccessDenied"

            # ListBuckets should work (allowed on *)
            resp = user_s3.list_buckets()
            assert len(resp["Buckets"]) >= 2
        finally:
            try:
                s3_root.delete_object(Bucket=bucket, Key="ok.txt")
                s3_root.delete_bucket(Bucket=bucket)
                s3_root.delete_bucket(Bucket=other_bucket)
            except Exception:
                pass
            self._cleanup_user(endpoint, creds, user_id, user_cred["access_key_id"])
            signed_request(
                "DELETE", f"{endpoint}/admin/grants/{grant_id}", creds
            )
