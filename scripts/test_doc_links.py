"""Deterministic regressions for the documentation checker (no public requests)."""

import importlib.util
import io
import json
from pathlib import Path
import sys
import subprocess
import tempfile
import time
import unittest
import urllib.error

SPEC = importlib.util.spec_from_file_location("doc_links", Path(__file__).with_name("check-doc-links.py"))
CHECK = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = CHECK
SPEC.loader.exec_module(CHECK)


class MarkdownTests(unittest.TestCase):
    def extract(self, source):
        return CHECK.links(Path("README.md"), source)

    def test_badges_balanced_escaped_angle_and_titles(self):
        links, errors = self.extract(r'''[![badge](image.svg)](https://example.org/project)
[one](file\(name\).md "title") [two](file(name).md)
[three](<file name.md#some-heading> 'title')
<https://example.org/guide>
''')
        self.assertEqual(errors, [])
        self.assertCountEqual([link.target for link in links], [
            "image.svg", "https://example.org/project", "file(name).md",
            "file(name).md", "file name.md#some-heading", "https://example.org/guide",
        ])

    def test_fences_comments_inline_code_and_escapes_are_not_links(self):
        links, errors = self.extract('''````markdown
```rust
[fake](missing.md)
```
````
~~~
[fake](missing.md)
~~~
<!-- [fake](missing.md) -->
`[fake](missing.md)` \\[fake](missing.md)

    [fake](missing.md)
[real](real.md)
''')
        self.assertEqual(errors, [])
        self.assertEqual([link.target for link in links], ["real.md"])

    def test_reference_styles_and_normalization(self):
        links, errors = self.extract('''[first][Some Label] [some label][] [SOME LABEL]
[Some   Label]:
  <guide.md#heading> "title"
[unmatched brackets]
''')
        self.assertEqual(errors, [])
        self.assertEqual([link.target for link in links], ["guide.md#heading"] * 4)

    def test_nested_lists_keep_links_and_nested_fences_hide_them(self):
        links, errors = self.extract('''- Parent
    - [child](child.md)
      continuation [next](next.md)

      ```
      [fake](missing.md)
      ```
''')
        self.assertEqual(errors, [])
        self.assertEqual([link.target for link in links], ["child.md", "next.md"])

    def test_stale_reference_and_malformed_destinations_fail(self):
        _, errors = self.extract('[stale][missing] [missing][] [bad](<unclosed)\n')
        self.assertEqual(len(errors), 3)
        self.assertIn("undefined reference", errors[0])
        self.assertIn("unclosed angle", errors[-1])

    def test_heading_collision_explicit_html_and_code(self):
        self.assertEqual(CHECK.anchors('''# Foo
# Foo-1
# Foo
## [Install][setup]
## **Café** &amp; `code`!
Setext title
------------
<a name="stable"></a><span id="other"></span>
`<a id="fake-inline"></a>`
```
# Fake
```
'''), {"foo", "foo-1", "foo-2", "install", "café--code", "setext-title", "stable", "other"})

    def test_heading_links_use_visible_labels_and_autolink_text(self):
        self.assertEqual(CHECK.anchors(r'''# [foo](bar(baz).md)
# [escaped](bar\(baz\).md "title")
# [angle](<file name.md> 'title')
# [![badge](image.svg)](target.md)
# <https://example.com/guide>
# `literal <span>`
'''), {"foo", "escaped", "angle", "badge", "httpsexamplecomguide", "literal-span"})
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            path = root / "guide.md"
            path.write_text('# [foo](bar(baz).md)\n# <https://example.com/guide>\n')
            for fragment in ["foo", "httpsexamplecomguide"]:
                self.assertIsNone(CHECK.local_error(root, CHECK.Link(path, 1, f"#{fragment}")))
            self.assertIsNotNone(CHECK.local_error(root, CHECK.Link(path, 1, "#foomd")))

    def test_stale_paths_anchors_percent_encoding_and_module_sections(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "guide.md").write_text('# Café\n# Title\n', encoding="utf-8")
            (root / "src").mkdir()
            (root / "src/lib.rs").write_text('//! # Middleware order\n')
            (root / "src/config.rs").write_text('//! # Configuration\n/// # Item only\n')
            source = root / "guide.md"
            for target in ["#caf%C3%A9", "guide.md?plain=1#title", "", "#"]:
                self.assertIsNone(CHECK.local_error(root, CHECK.Link(source, 1, target)))
            for target in ["missing.md", "guide.md#missing", "../escape.md"]:
                self.assertIsNotNone(CHECK.local_error(root, CHECK.Link(source, 1, target)))
            source = root / "src/config.rs"
            self.assertIsNone(CHECK.local_error(root, CHECK.Link(source, 1, "crate#middleware-order")))
            self.assertIsNone(CHECK.local_error(root, CHECK.Link(source, 1, "crate::config#configuration")))
            self.assertIsNone(CHECK.local_error(root, CHECK.Link(source, 1, "Self::symbol")))
            self.assertIsNotNone(CHECK.local_error(root, CHECK.Link(source, 1, "crate::config#stale")))
            self.assertIsNotNone(CHECK.local_error(root, CHECK.Link(source, 1, "crate::config#item-only")))
            github = "https://github.com/joshrotenberg/mcp-proxy/blob/main/"
            self.assertIsNone(CHECK.local_error(root, CHECK.Link(source, 1, github + "guide.md#title")))
            self.assertIsNotNone(CHECK.local_error(root, CHECK.Link(source, 1, github + "guide.md#stale")))


class Response(io.BytesIO):
    status = 200
    url = "https://docs.rs/mcp-proxy/0.6.0/mcp_proxy/"
    headers = {"Content-Type": "text/html"}


class Opener:
    def __init__(self, response):
        self.response = response

    def open(self, request, timeout):
        if isinstance(self.response, Exception):
            raise self.response
        return self.response


class ReportTests(unittest.TestCase):
    def test_stalled_probe_and_unvisited_urls_obey_wall_clock_budget(self):
        # A spawned worker stalls like slow DNS/body I/O; no network is involved.
        checker = str(Path(CHECK.__file__).resolve())
        helper = f'''import importlib.util, json, sys, time
spec = importlib.util.spec_from_file_location("doc_links", {checker!r})
check = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = check
spec.loader.exec_module(check)
def stall(*args):
    time.sleep(30)
check.worker = stall
if __name__ == "__main__":
    print(json.dumps(check.external_report(["https://example.org/a", "https://example.org/b"], timeout=.15, budget=.1)))
'''
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "probe.py"
            path.write_text(helper)
            start = time.monotonic()
            result = subprocess.run([sys.executable, str(path)], capture_output=True, text=True, timeout=3, check=True)
            self.assertLess(time.monotonic() - start, 2)
        entries = json.loads(result.stdout)
        self.assertEqual(entries[0]["reason"], "probe wall-clock deadline")
        self.assertEqual(entries[1]["status"], "not_checked")

    def test_final_version_and_fragment_observed(self):
        result = CHECK.observe("https://docs.rs/mcp-proxy/latest/mcp_proxy/#anchor", 1, 100,
                               Opener(Response(b'<h1 id="anchor">Title</h1>')))
        self.assertEqual(result["status"], "available")
        self.assertEqual(result["fragment"], "found")
        self.assertTrue(result["requested_version"]["floating"])
        self.assertEqual(result["final_version"]["version"], "0.6.0")

    def test_docs_rs_crate_info_version_hints(self):
        for prefix in ["https://docs.rs/mcp-proxy", "https://docs.rs/crate/mcp-proxy"]:
            for version in ["latest", "0.6.0"]:
                hint = CHECK.version_hint(f"{prefix}/{version}")
                self.assertEqual(hint["crate"], "mcp-proxy")
                self.assertEqual(hint["version"], version)
                self.assertEqual(hint["floating"], version == "latest")
        self.assertIsNone(CHECK.version_hint("https://docs.rs/crate/"))

    def test_missing_fragment_is_not_missing_when_truncated(self):
        url = "https://example.org/#anchor"
        result = CHECK.observe(url, 1, 10, Opener(Response(b"<html>" + b"x" * 20)))
        self.assertEqual(result["fragment"], "not_checked_truncated")
        self.assertTrue(result["truncated"])
        result = CHECK.observe(url, 1, 100, Opener(Response(b"<html></html>")))
        self.assertEqual(result["fragment"], "missing")

    def test_non_html_fragments_and_badges_do_not_imply_verification(self):
        response = Response(b"not html")
        response.headers = {"Content-Type": "application/pdf"}
        result = CHECK.observe("https://example.org/file#page-1", 1, 100, Opener(response))
        self.assertEqual(result["fragment"], "not_checked_non_html")
        self.assertIsNone(CHECK.version_hint("https://docs.rs/mcp-proxy/badge.svg"))

    def test_rate_limit_not_available_and_no_retries(self):
        url = "https://example.org/"
        for status, expected in [(429, "rate_limited"), (404, "unavailable"), (503, "unavailable")]:
            error = urllib.error.HTTPError(url, status, "no", {"Retry-After": "60"}, io.BytesIO())
            result = CHECK.observe(url, 1, 100, Opener(error))
            self.assertEqual(result["status"], expected)
            self.assertEqual(result["http_status"], status)
            if status == 429:
                self.assertEqual(result["retry_after"], "60")

    def test_timeout_and_nonpublic_urls_are_not_success(self):
        result = CHECK.observe("https://example.org/", 1, 100, Opener(TimeoutError("deadline")))
        self.assertEqual(result["status"], "unavailable")
        for url in ["http://localhost/", "http://127.0.0.1/", "file:///etc/passwd", "https://user:pass@example.org/"]:
            self.assertEqual(CHECK.observe(url, 1, 100)["status"], "not_checked")

    def test_redirects_reject_nonpublic_destinations(self):
        redirect = CHECK.Redirects()
        request = CHECK.urllib.request.Request("https://example.org/")
        with self.assertRaises(urllib.error.URLError):
            redirect.redirect_request(request, None, 302, "Found", {}, "http://127.0.0.1/private")


if __name__ == "__main__":
    unittest.main()
