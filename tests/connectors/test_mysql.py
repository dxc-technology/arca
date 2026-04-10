"""Integration tests for the MySQL notification connector."""

import json
import os
import time

import pymysql
import pytest

from conftest import sigv4_request


MYSQL_URL = os.environ.get(
    "MYSQL_URL", "mysql://arca:arca@mysql-receiver:3306/arca_test"
)
ARCA_ENDPOINT = os.environ.get("ARCA_ENDPOINT", "http://arca:9000")

DEFAULT_TABLE = "arca_notifications"


def _parse_mysql_url(url):
    """Parse mysql://user:pass@host:port/db into connection kwargs."""
    stripped = url.replace("mysql://", "")
    userinfo, rest = stripped.split("@", 1)
    user, password = userinfo.split(":", 1)
    hostport, db = rest.split("/", 1)
    host, port = hostport.split(":", 1)
    return dict(host=host, port=int(port), user=user, password=password, database=db)


def _mysql_connect():
    """Open a pymysql connection to the MySQL receiver."""
    kwargs = _parse_mysql_url(MYSQL_URL)
    return pymysql.connect(**kwargs)


def _wait_for_row(table, key, timeout=10):
    """Poll the table until a row with the given object key appears."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        conn = _mysql_connect()
        try:
            with conn.cursor() as cur:
                cur.execute(
                    f"SELECT payload FROM {table} WHERE `key` = %s", (key,)
                )
                row = cur.fetchone()
                if row:
                    return json.loads(row[0])
        except pymysql.err.ProgrammingError:
            pass
        finally:
            conn.close()
        time.sleep(0.5)
    return None


def put_notification_config_xml(bucket, mysql_url, table=None, events=None):
    """Configure a bucket notification with MySQL connector via raw XML."""
    event_elements = ""
    for evt in (events or ["s3:ObjectCreated:*"]):
        event_elements += f"    <Event>{evt}</Event>\n"

    properties = ""
    if table:
        properties += f'    <Property><Name>table</Name><Value>{table}</Value></Property>\n'

    xml = f"""<?xml version="1.0" encoding="UTF-8"?>
<NotificationConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <TopicConfiguration>
    <Id>mysql-test</Id>
    <Topic>{mysql_url}</Topic>
{event_elements}    <ConnectorType>mysql</ConnectorType>
{properties}  </TopicConfiguration>
</NotificationConfiguration>"""

    url = f"{ARCA_ENDPOINT}/{bucket}?notification"
    resp = sigv4_request("PUT", url, data=xml, headers={"Content-Type": "application/xml"})
    assert resp.status_code in (200, 204), f"PUT notification config failed: {resp.status_code} {resp.text}"


class TestMysqlDelivery:
    """Tests for MySQL event delivery."""

    def test_put_object_inserts_event(self, s3_client, unique_bucket):
        """PutObject should insert a row into the default table."""
        put_notification_config_xml(unique_bucket, MYSQL_URL)
        s3_client.put_object(Bucket=unique_bucket, Key="hello.txt", Body=b"world")

        payload = _wait_for_row(DEFAULT_TABLE, "hello.txt")
        assert payload is not None
        records = payload.get("Records", [])
        assert len(records) >= 1
        assert records[0]["eventName"] == "s3:ObjectCreated:Put"
        assert records[0]["s3"]["bucket"]["name"] == unique_bucket

    def test_custom_table_name(self, s3_client, unique_bucket):
        """Events should be inserted into the table specified in properties."""
        custom_table = "custom_events"
        put_notification_config_xml(unique_bucket, MYSQL_URL, table=custom_table)
        s3_client.put_object(Bucket=unique_bucket, Key="custom.txt", Body=b"data")

        payload = _wait_for_row(custom_table, "custom.txt")
        assert payload is not None

    def test_delete_object_inserts_event(self, s3_client, unique_bucket):
        """DeleteObject should insert an s3:ObjectRemoved event."""
        put_notification_config_xml(
            unique_bucket, MYSQL_URL,
            events=["s3:ObjectCreated:*", "s3:ObjectRemoved:*"],
        )
        s3_client.put_object(Bucket=unique_bucket, Key="to-delete.txt", Body=b"bye")
        time.sleep(1)
        s3_client.delete_object(Bucket=unique_bucket, Key="to-delete.txt")

        deadline = time.time() + 10
        while time.time() < deadline:
            conn = _mysql_connect()
            try:
                with conn.cursor() as cur:
                    cur.execute(
                        f"SELECT payload FROM {DEFAULT_TABLE} WHERE event_name LIKE %s AND `key` = %s",
                        ("%ObjectRemoved%", "to-delete.txt"),
                    )
                    row = cur.fetchone()
                    if row:
                        payload = json.loads(row[0])
                        assert "ObjectRemoved" in payload["Records"][0]["eventName"]
                        return
            except pymysql.err.ProgrammingError:
                pass
            finally:
                conn.close()
            time.sleep(0.5)
        pytest.fail("Delete event not found in MySQL")

    def test_event_payload_format(self, s3_client, unique_bucket):
        """Verify the inserted payload matches the S3 event record format."""
        put_notification_config_xml(unique_bucket, MYSQL_URL)
        s3_client.put_object(Bucket=unique_bucket, Key="format.txt", Body=b"test data")

        payload = _wait_for_row(DEFAULT_TABLE, "format.txt")
        assert payload is not None
        record = payload["Records"][0]
        assert record["eventVersion"] == "2.1"
        assert record["eventSource"] == "arca:s3"
        assert "eventTime" in record
        assert record["s3"]["object"]["key"] == "format.txt"

    def test_auto_creates_table(self, s3_client, unique_bucket):
        """The connector should auto-create the table if it does not exist."""
        auto_table = f"auto_{unique_bucket.replace('-', '_')}"
        put_notification_config_xml(unique_bucket, MYSQL_URL, table=auto_table)
        s3_client.put_object(Bucket=unique_bucket, Key="auto.txt", Body=b"data")

        payload = _wait_for_row(auto_table, "auto.txt")
        assert payload is not None


class TestMysqlConnectivity:
    """Tests for the admin API connector test endpoint."""

    def test_mysql_connectivity_success(self):
        """Admin API test-connector with valid MySQL URL should succeed."""
        url = f"{ARCA_ENDPOINT}/admin/notifications/test-connector"
        body = json.dumps({
            "connector_type": "mysql",
            "url": MYSQL_URL,
            "properties": {},
        })
        resp = sigv4_request("POST", url, data=body, headers={"Content-Type": "application/json"})
        assert resp.status_code == 200
        data = resp.json()
        assert data["success"] is True
        assert data["connector_type"] == "mysql"

    def test_mysql_connectivity_failure(self):
        """Admin API test-connector with bad MySQL URL should fail gracefully."""
        url = f"{ARCA_ENDPOINT}/admin/notifications/test-connector"
        body = json.dumps({
            "connector_type": "mysql",
            "url": "mysql://bad:bad@nonexistent-host:3306/nope",
            "properties": {},
        })
        resp = sigv4_request("POST", url, data=body, headers={"Content-Type": "application/json"})
        assert resp.status_code == 200
        data = resp.json()
        assert data["success"] is False
        assert data["error"] is not None
