import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from refresh_projects import ALIASES, BASE, crawl, page_url, parse_page, refresh, atomic_write


def page(name="owner/repo", stars="1,234", next_url=None):
    pagination = f'<div data-test-selector="pagination"><a href="{next_url}">Next</a></div>' if next_url else ""
    return f'''<div data-test-id='dg-repo-pkg-dependent'><a href='/{name}' data-hovercard-type='repository'>repo</a><span><svg class='octicon octicon-star'><path /></svg>{stars}</span></div>{pagination}'''


class RefreshTests(unittest.TestCase):
    def test_parser_handles_html_entities_attribute_order_and_rounded_stars(self):
        rows, next_url, _ = parse_page(page(stars="1.2k", next_url="?a=1&amp;b=2"))
        self.assertEqual(rows[0]["stars"], 1200)
        self.assertEqual(next_url, "?a=1&b=2")

    def test_pagination_and_case_insensitive_alias_deduplication(self):
        calls = []
        def fetch(url):
            calls.append(url)
            if "dependents_after=" in url:
                return page("other/repo", "2")
            if ALIASES[0] in url:
                return page("Owner/Repo", next_url="?dependents_after=cursor")
            return page("owner/repo", "42")
        repos, _, pages = crawl(fetch)
        self.assertEqual(len(repos), 2)
        self.assertEqual(repos["owner/repo"]["stars"], 42)
        self.assertEqual(pages, {ALIASES[0]: 2, ALIASES[1]: 1})
        self.assertEqual(len(calls), 3)
        self.assertIn(ALIASES[0], calls[1])

    def test_rejects_cycles_limits_foreign_urls_and_error_pages(self):
        with self.assertRaises(ValueError):
            crawl(lambda _: page(next_url=BASE), aliases=ALIASES[:1])
        with self.assertRaises(ValueError):
            crawl(lambda _: page(next_url="?dependents_after=next"), max_pages=1)
        with self.assertRaises(ValueError):
            page_url("https://example.com/next", ALIASES[0])
        with self.assertRaises(ValueError):
            parse_page("<html>Please sign in</html>")

    def test_failed_atomic_replace_retains_cache_and_removes_temporary_file(self):
        with tempfile.TemporaryDirectory() as directory:
            cache = Path(directory) / "cache.json"
            cache.write_text("previous")
            with patch("refresh_projects.os.replace", side_effect=OSError("disk")):
                with self.assertRaises(OSError):
                    atomic_write(cache, {"next": True})
            self.assertEqual(cache.read_text(), "previous")
            self.assertEqual(list(Path(directory).iterdir()), [cache])

    def test_repeated_page_contents_do_not_claim_complete_discovery(self):
        with self.assertRaisesRegex(ValueError, "repeated repository"):
            crawl(lambda url: page(next_url="?dependents_after=" + str(len(url))))

    def test_failure_retains_cache_byte_for_byte(self):
        with tempfile.TemporaryDirectory() as directory:
            cache, curated = Path(directory) / "cache.json", Path(directory) / "curated.json"
            cache.write_text('{"previous": true}\n')
            curated.write_text("[]")
            with self.assertRaises(OSError):
                refresh(cache, curated, lambda _: (_ for _ in ()).throw(OSError("network")))
            self.assertEqual(cache.read_text(), '{"previous": true}\n')

    def test_success_is_dated_sorted_deduplicated_and_preserves_curated_sources(self):
        with tempfile.TemporaryDirectory() as directory:
            cache, curated = Path(directory) / "cache.json", Path(directory) / "curated.json"
            curated.write_text(json.dumps([{"name": "owner/repo", "url": "https://github.com/owner/repo", "usage_url": "https://github.com/owner/repo/blob/sha/workflow.yml"}]))
            def fetch(url):
                return json.dumps({"stargazers_count": 9999}) if "api.github.com" in url else page()
            result = refresh(cache, curated, fetch)
            self.assertEqual(result["repository_count"], 1)
            self.assertEqual(result["projects"][0]["stars"], 9999)
            self.assertIn("/blob/sha/", result["projects"][0]["usage_url"])
            self.assertTrue(result["updated_at"].endswith("Z"))
            self.assertEqual(json.loads(cache.read_text()), result)


if __name__ == "__main__":
    unittest.main()
