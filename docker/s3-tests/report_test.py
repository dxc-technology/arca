"""Tests for report.py's AWS-divergence reclassification.

Run inside the s3-tests image (it has pytest):
    docker run --rm -v "$PWD/docker/s3-tests:/t" -w /t --entrypoint pytest arca-s3-tests report_test.py
"""

import json

import report


def junit(*cases: str) -> str:
    return '<?xml version="1.0"?><testsuites><testsuite name="s3">' + "".join(cases) + "</testsuite></testsuites>"


def failed(name: str, message: str) -> str:
    return (f'<testcase classname="s3tests.functional.test_s3" name="{name}" time="0.1">'
            f'<failure message="{message}">trace</failure></testcase>')


def passed(name: str) -> str:
    return f'<testcase classname="s3tests.functional.test_s3" name="{name}" time="0.1"/>'


DIVERGENT_MESSAGE = "AssertionError: assert 'InvalidRequest' == 'BadDigest'"


def parse(tmp_path, xml: str) -> list:
    path = tmp_path / "results.xml"
    path.write_text(xml)
    return report.parse_junit_xml(str(path))


def test_known_divergence_on_its_exact_assertion_counts_as_passed(tmp_path):
    [t] = parse(tmp_path, junit(failed("test_object_checksum_sha256", DIVERGENT_MESSAGE)))
    assert t["status"] == "passed"
    assert t["aws_divergence"]
    assert "InvalidRequest" in t["aws_divergence"]


def test_known_divergence_failing_on_another_assertion_stays_failed(tmp_path):
    # Any other failure in the same test is a real one: never hide it.
    [t] = parse(tmp_path, junit(failed("test_object_checksum_sha256",
                                       "AssertionError: assert 'abc' == 'arcu6553sHVA'")))
    assert t["status"] == "failed"
    assert t["aws_divergence"] is None


def test_divergent_assertion_text_in_an_unlisted_test_stays_failed(tmp_path):
    [t] = parse(tmp_path, junit(failed("test_multipart_checksum_sha256", DIVERGENT_MESSAGE)))
    assert t["status"] == "failed"
    assert t["aws_divergence"] is None


def test_errors_are_never_reclassified(tmp_path):
    xml = junit('<testcase classname="c" name="test_object_checksum_crc64nvme" time="0.1">'
                f'<error message="{DIVERGENT_MESSAGE}">trace</error></testcase>')
    [t] = parse(tmp_path, xml)
    assert t["status"] == "error"
    assert t["aws_divergence"] is None


def test_ordinary_results_carry_no_divergence(tmp_path):
    [t] = parse(tmp_path, junit(passed("test_bucket_list_empty")))
    assert t["status"] == "passed"
    assert t["aws_divergence"] is None


def test_summary_counts_and_lists_divergences(tmp_path):
    tests = parse(tmp_path, junit(
        failed("test_object_checksum_sha256", DIVERGENT_MESSAGE),
        failed("test_object_checksum_crc64nvme", DIVERGENT_MESSAGE),
        passed("test_bucket_list_empty"),
    ))
    out = tmp_path / "summary.json"
    report.generate_summary_json(tests, str(out))
    summary = json.loads(out.read_text())
    assert summary["passed"] == 3
    assert summary["aws_divergences"] == ["test_object_checksum_crc64nvme", "test_object_checksum_sha256"]


def test_html_shows_divergences_with_their_reason(tmp_path):
    tests = parse(tmp_path, junit(failed("test_object_checksum_sha256", DIVERGENT_MESSAGE)))
    out = tmp_path / "report.html"
    report.generate_html(tests, str(out))
    html = out.read_text()
    assert "test_object_checksum_sha256" in html
    assert report.AWS_DIVERGENCES["test_object_checksum_sha256"]["reason"] in html
