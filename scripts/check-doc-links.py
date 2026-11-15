#!/usr/bin/env python3
"""Check repository prose links; optionally write bounded external observations.

This deliberately small Markdown subset is documented in docs/doc-links.md.
Rustdoc still owns symbol resolution. No network is used without --external.
"""

import argparse
from collections import Counter
from dataclasses import dataclass
from datetime import datetime, timezone
import html
from html.parser import HTMLParser
import ipaddress
import json
import multiprocessing
from pathlib import Path
import re
import subprocess
import time
import tomllib
import urllib.error
import urllib.parse
import urllib.request


@dataclass(frozen=True)
class Link:
    path: Path
    line: int
    target: str


def blank(text):
    return re.sub(r"[^\n]", " ", text)


def doc_text(path, module_only=False):
    text = path.read_text(encoding="utf-8")
    if path.suffix == ".rs":
        return "\n".join(
            match[1] if (match := re.match(r"\s*//" + ("!" if module_only else "[/!]") + r" ?(.*)", line)) else ""
            for line in text.splitlines()
        )
    return text


def prose(text):
    """Mask code/comments without changing offsets or source line numbers."""
    text = re.sub(r"<!--.*?-->", lambda m: blank(m[0]), text, flags=re.S)
    result, fence = [], None
    list_indents, previous_blank, indented_code = [], True, False
    for line in text.splitlines(keepends=True):
        indent = len(line) - len(line.lstrip(" \t"))
        nonblank = bool(line.strip())
        if nonblank:
            while list_indents and indent < list_indents[-1]:
                list_indents.pop()
        item = re.match(r" *(?:[-+*]|\d+[.)]) +", line)
        if item and not indented_code:
            list_indents.append(item.end())
        content = line[item.end():] if item else line[(list_indents[-1] if list_indents else 0):]
        opening = re.match(r" {0,3}(`{3,}|~{3,})(.*)", content)
        if fence:
            if re.match(r" {0,3}" + re.escape(fence[0]) + r"{" + str(len(fence)) + r",}\s*$", content):
                fence = None
            result.append(blank(line))
        elif opening:
            fence = opening[1]
            result.append(blank(line))
        elif (previous_blank or indented_code) and not item and indent >= (list_indents[-1] if list_indents else 0) + 4:
            result.append(blank(line))
            indented_code = True
        else:
            result.append(line)
            if nonblank:
                indented_code = False
        previous_blank = not nonblank
    return "".join(result)


def mask_inline_code(text):
    return re.sub(r"(`+)(?!`)(.*?)\1(?!`)", lambda m: blank(m[0]), text, flags=re.S)


def unescape(text):
    return html.unescape(re.sub(r"\\([!\"#$%&'()*+,\-./:;<=>?@\[\]\\^_`{|}~])", r"\1", text))


def label(text):
    return " ".join(unescape(text).split()).casefold()


def destination(text, start):
    """Read an angle or balanced bare destination and return its end offset."""
    while start < len(text) and text[start].isspace():
        start += 1
    if start < len(text) and text[start] == "<":
        end = start + 1
        while end < len(text):
            if text[end] == "\\":
                end += 2
            elif text[end] == ">":
                return unescape(text[start + 1:end]), end + 1
            elif text[end] == "\n":
                break
            else:
                end += 1
        raise ValueError("unclosed angle destination")
    end, depth = start, 0
    while end < len(text):
        char = text[end]
        if char == "\\":
            end += 2
            continue
        if char == "(":
            depth += 1
        elif char == ")":
            if not depth:
                break
            depth -= 1
        elif char.isspace():
            break
        end += 1
    if depth:
        raise ValueError("unbalanced destination parentheses")
    return unescape(text[start:end]), end


def closing_bracket(text, start):
    end, depth = start + 1, 1
    while end < len(text) and depth:
        if text[end] == "\\":
            end += 2
            continue
        if text[end] == "[":
            depth += 1
        elif text[end] == "]":
            depth -= 1
        end += 1
    return None if depth else end


def inline_destination(text, start):
    target, stop = destination(text, start)
    # Optional title, followed by the closing parenthesis.
    tail = re.match(r"\s*(?:(?:\"[^\"]*\"|'[^']*'|\([^)]*\))\s*)?\)", text[stop:])
    if not tail:
        raise ValueError("unclosed inline destination/title")
    return target, stop + tail.end()


def links(path, text):
    text = prose(text)
    found, errors, definitions = [], [], {}
    # A definition may put its destination on the immediately following line.
    pattern = re.compile(r"^ {0,3}\[([^]\n]+)\]:[ \t]*(?:\n[ \t]*)?", re.M)
    for match in list(pattern.finditer(text)):
        line = text.count("\n", 0, match.start()) + 1
        try:
            target, end = destination(text, match.end())
            if not target and text[match.end():end].strip() != "<>":
                raise ValueError("empty reference destination")
            key = label(match[1])
            if key in definitions:
                raise ValueError(f"duplicate reference definition: {match[1]}")
            definitions[key] = target
            found.append(Link(path, line, target))
            text = text[:match.start()] + blank(text[match.start():end]) + text[end:]
        except ValueError as error:
            errors.append(f"{path}:{line}: {error}")
    masked = mask_inline_code(text)
    for match in re.finditer(r"(?<!\\)\[", masked):
        start = match.start()
        if start and masked[start - 1] == "]":
            continue  # The second bracket pair of a full/collapsed reference.
        end = closing_bracket(masked, start)
        if end is None:
            continue  # Plain unmatched brackets are legal prose.
        line = text.count("\n", 0, start) + 1
        name = text[start + 1:end - 1]
        try:
            if end < len(text) and text[end] == "(":
                target, _ = inline_destination(text, end + 1)
                found.append(Link(path, line, target))
            elif end < len(text) and text[end] == "[":
                stop = text.find("]", end + 1)
                if stop < 0:
                    raise ValueError("unclosed reference label")
                key = label(text[end + 1:stop] or name)
                if key not in definitions:
                    raise ValueError(f"undefined reference: {key}")
                found.append(Link(path, line, definitions[key]))
            elif label(name) in definitions:
                found.append(Link(path, line, definitions[label(name)]))
        except ValueError as error:
            errors.append(f"{path}:{line}: {error}")
    for match in re.finditer(r"<(https?://[^<>\s]+)>", masked):
        found.append(Link(path, text.count("\n", 0, match.start()) + 1, unescape(match[1])))
    return found, errors


class Anchors(HTMLParser):
    def __init__(self):
        super().__init__()
        self.ids = set()

    def handle_starttag(self, tag, attrs):
        for key, value in attrs:
            if value is not None and (key == "id" or (tag == "a" and key == "name")):
                self.ids.add(value)


def heading_links(text):
    """Keep visible link labels, using the same balanced parser as inventory."""
    masked = mask_inline_code(text)
    result, cursor = [], 0
    for match in re.finditer(r"(?<!\\)\[", masked):
        start = match.start()
        if start < cursor:
            continue
        end = closing_bracket(masked, start)
        if end is None:
            continue
        stop = None
        if end < len(text) and text[end] == "(":
            try:
                _, stop = inline_destination(text, end + 1)
            except ValueError:
                continue  # links() reports malformed destinations separately.
        elif end < len(text) and text[end] == "[":
            closing = text.find("]", end + 1)
            if closing >= 0:
                stop = closing + 1
        if stop is not None:
            result.extend((text[cursor:start], heading_links(text[start + 1:end - 1])))
            cursor = stop
    result.append(text[cursor:])
    return "".join(result)


def anchors(text):
    text = prose(text)
    parser = Anchors()
    parser.feed(mask_inline_code(text))
    result = parser.ids
    headings = []
    lines = text.splitlines()
    for index, line in enumerate(lines):
        if match := re.match(r" {0,3}#{1,6}\s+(.*?)\s*#*\s*$", line):
            headings.append(match[1])
        elif index and re.fullmatch(r" {0,3}(?:=+|-+)\s*", line) and lines[index - 1].strip():
            headings.append(lines[index - 1].strip())
    for heading in headings:
        heading = heading_links(heading)
        # Code and autolinks display their contents, rather than HTML tags.
        heading = re.sub(r"(`+)(?!`)(.*?)\1(?!`)", lambda m: html.escape(m[2]), heading)
        heading = re.sub(r"<(https?://[^<>\s]+)>", lambda m: html.escape(m[1]), heading)
        heading = html.unescape(re.sub(r"<[^>]+>", "", heading)).lower()
        slug = "".join(c for c in heading if c.isalnum() or c in "_ -")
        slug = slug.replace(" ", "-")
        candidate, suffix = slug, 0
        while candidate in result:
            suffix += 1
            candidate = f"{slug}-{suffix}"
        result.add(candidate)
    return result


def local_error(root, link):
    rust_symbol = link.path.suffix == ".rs" and "::" in link.target
    target = urllib.parse.urlsplit(link.target if not rust_symbol else "")
    if target.hostname == "github.com" and target.path.startswith((
        "/joshrotenberg/mcp-proxy/blob/main/", "/joshrotenberg/mcp-proxy/tree/main/"
    )):
        path = "/" + target.path.split("/", 5)[5]
        target = target._replace(scheme="", netloc="", path=path)
    if target.scheme in {"http", "https", "mailto"} or target.netloc:
        return None
    path = urllib.parse.unquote(link.target.split("#", 1)[0] if rust_symbol else target.path)
    fragment = urllib.parse.unquote(link.target.partition("#")[2] if rust_symbol else target.fragment)
    module = False
    if link.path.suffix == ".rs" and ("::" in path or re.fullmatch(r"\w+", path)):
        # Rustdoc resolves API symbols; we additionally check module sections.
        if not fragment:
            return None
        parts = path.removeprefix("crate::").split("::")
        if parts == ["crate"]:
            dest = root / "src/lib.rs"
        else:
            dest = root / "src" / ("/".join(parts) + ".rs")
        module = True
    elif target.scheme:
        return f"unsupported URL scheme: {target.scheme}"
    else:
        dest = root / path.lstrip("/") if path.startswith("/") else link.path.parent / path
        if not path:
            dest = link.path
            module = dest.suffix == ".rs"
    dest = dest.resolve()
    if not dest.is_relative_to(root.resolve()):
        return "destination escapes repository"
    if not dest.exists():
        return "missing local path"
    if not fragment:
        return None
    if dest.is_dir():
        dest = dest / "README.md"
        if not dest.is_file():
            return "directory fragment has no README.md"
    if not module and dest.suffix != ".md":
        if match := re.fullmatch(r"L(\d+)(?:-L(\d+))?", fragment):
            first, last = int(match[1]), int(match[2] or match[1])
            if 0 < first <= last <= len(dest.read_text(encoding="utf-8").splitlines()):
                return None
            return "source line anchor out of range"
        return "fragment on non-Markdown source must use #L<number>"
    if fragment not in anchors(doc_text(dest, module_only=module)):
        return f"missing anchor #{fragment}"
    return None


def version_hint(url):
    parsed = urllib.parse.urlsplit(url)
    parts = parsed.path.strip("/").split("/")
    if parsed.hostname == "docs.rs" and parts:
        if parts[0] == "crate":
            parts = parts[1:]
        if not parts or not parts[0]:
            return None
        if len(parts) > 1 and parts[1] == "badge.svg":
            return None
        version = parts[1] if len(parts) > 1 and parts[1] != "badge.svg" else "unspecified"
        return {"site": "docs.rs", "crate": parts[0], "version": version,
                "floating": version in {"latest", "unspecified"}}
    if parsed.hostname == "github.com" and len(parts) > 3 and parts[2] in {"blob", "tree"}:
        return {"site": "github.com", "ref": parts[3], "floating": parts[3] in {"main", "master", "HEAD"}}
    return None


def public_url(url):
    parsed = urllib.parse.urlsplit(url)
    host = parsed.hostname or ""
    if parsed.scheme not in {"http", "https"} or not host or parsed.username or parsed.password:
        return False
    if host == "localhost" or host.endswith((".localhost", ".local")):
        return False
    try:
        return ipaddress.ip_address(host).is_global
    except ValueError:
        return "." in host


class Redirects(urllib.request.HTTPRedirectHandler):
    max_redirections = 5
    max_repeats = 1

    def redirect_request(self, request, fp, code, msg, headers, newurl):
        if not public_url(newurl):
            raise urllib.error.URLError("redirect is not a public HTTP(S) URL")
        return super().redirect_request(request, fp, code, msg, headers, newurl)


def observe(url, timeout, max_bytes, opener=None):
    """One attempt, bounded bytes; caller enforces a separate wall-clock limit."""
    requested, fragment = urllib.parse.urldefrag(url)
    result = {"url": url, "requested_version": version_hint(url)}
    if not public_url(url):
        return result | {"status": "not_checked", "reason": "non-public or credential-bearing URL"}
    opener = opener or urllib.request.build_opener(Redirects())
    request = urllib.request.Request(requested, headers={"User-Agent": "mcp-proxy-doc-links/1.0"})
    try:
        with opener.open(request, timeout=timeout) as response:
            body = response.read(max_bytes + 1)
            result.update(status="available", http_status=response.status, final_url=response.url,
                          final_version=version_hint(response.url), truncated=len(body) > max_bytes)
            if fragment:
                if "html" not in response.headers.get("Content-Type", ""):
                    result["fragment"] = "not_checked_non_html"
                else:
                    parser = Anchors()
                    parser.feed(body[:max_bytes].decode("utf-8", errors="replace"))
                    result["fragment"] = ("found" if urllib.parse.unquote(fragment) in parser.ids
                                          else "not_checked_truncated" if len(body) > max_bytes
                                          else "missing")
    except urllib.error.HTTPError as error:
        result.update(status="rate_limited" if error.code == 429 else "unavailable", http_status=error.code,
                      final_url=error.url, final_version=version_hint(error.url))
        if error.code == 429:
            result["retry_after"] = error.headers.get("Retry-After")
        error.close()
    except (urllib.error.URLError, OSError, ValueError) as error:
        result.update(status="unavailable", reason=str(error))
    return result


def worker(pipe, url, timeout, max_bytes):
    try:
        pipe.send(observe(url, timeout, max_bytes))
    finally:
        pipe.close()


def external_report(urls, max_urls=100, timeout=5, budget=120, max_bytes=262144):
    deadline = time.monotonic() + budget
    results = []
    context = multiprocessing.get_context("spawn")
    for index, url in enumerate(sorted(set(urls))):
        remaining = deadline - time.monotonic()
        if index >= max_urls or remaining <= 0:
            results.append({"url": url, "status": "not_checked", "reason": "report budget exhausted",
                            "requested_version": version_hint(url)})
            continue
        read, write = context.Pipe(duplex=False)
        process = context.Process(target=worker, args=(write, url, timeout, max_bytes))
        process.start()
        write.close()
        try:
            if read.poll(min(timeout, remaining)):
                try:
                    results.append(read.recv())
                except EOFError:
                    results.append({"url": url, "status": "unavailable", "reason": "probe process exited"})
            else:
                results.append({"url": url, "status": "unavailable", "reason": "probe wall-clock deadline"})
        finally:
            if process.is_alive():
                process.terminate()
            process.join(timeout=0.1)
            if process.is_alive():
                process.kill()
                process.join(timeout=0.1)
            read.close()
    return results


def sources(root):
    names = subprocess.check_output(["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"], cwd=root).decode().split("\0")
    return sorted({root / name for name in names if name.endswith(".md") or name.startswith("src/") and name.endswith(".rs")})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--external", action="store_true")
    parser.add_argument("--report", type=Path, default=Path("target/doc-links.json"))
    parser.add_argument("--max-urls", type=int, default=100)
    parser.add_argument("--timeout", type=float, default=5)
    parser.add_argument("--budget", type=float, default=120)
    args = parser.parse_args()
    if not 1 <= args.max_urls <= 500 or not 0 < args.timeout <= 30 or not 0 < args.budget <= 600:
        parser.error("limits require 1..500 URLs, 0..30s per URL and 0..600s overall")
    root = args.root.resolve()
    all_links, errors = [], []
    for path in sources(root):
        extracted, failures = links(path, doc_text(path))
        all_links.extend(extracted)
        errors.extend(failures)
    urls = {}
    for link in all_links:
        if error := local_error(root, link):
            errors.append(f"{link.path.relative_to(root)}:{link.line}: {link.target}: {error}")
        if link.target.startswith(("http://", "https://")):
            urls.setdefault(link.target, set()).add(f"{link.path.relative_to(root)}:{link.line}")
    for error in errors:
        print(error)
    print(f"Checked {len(all_links)} prose destinations; {len(urls)} unique external URLs; {len(errors)} local errors.")
    if args.external:
        observations = external_report(urls, args.max_urls, args.timeout, args.budget)
        for result in observations:
            result["sources"] = sorted(urls[result["url"]])
        report = {"schema_version": 1, "generated_at": datetime.now(timezone.utc).isoformat(),
                  "commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root).decode().strip(),
                  "package_version": tomllib.loads((root / "Cargo.toml").read_text())["package"]["version"],
                  "limits": {"max_urls": args.max_urls, "per_url_seconds": args.timeout, "total_seconds": args.budget,
                             "max_body_bytes": 262144, "max_redirects": 5, "retries": 0},
                  "limitations": "Observations only; availability is not correctness, matching release content or external deployment verification.",
                  "local_errors": errors, "external": observations}
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(report, indent=2) + "\n")
        print(f"External report: {args.report}; statuses: {dict(Counter(r['status'] for r in observations))}")
    else:
        print("External availability, redirects, versions and fragments were not checked (use --external).")
    return bool(errors)


if __name__ == "__main__":
    raise SystemExit(main())
