# Project snapshots

Run `npm run refresh-projects` in `website` to discover dependents of both Action
names, follow every pagination cursor, deduplicate repository names, refresh
maintained examples' star counts, and atomically replace the dated JSON snapshot.
`GITHUB_TOKEN` is optional and is used only for public repository metadata queries.

The normal website build reads the committed cache and never requires GitHub.
Website CD attempts a refresh before building; failures retain the previous cache.
Commit refreshed data when updating the baseline used by offline/failed builds.

GitHub's dependents pages are HTML, so markup changes, pagination cycles, exhausted
page limits, rate limits, and missing metadata are treated as refresh failures.
Discovery includes public repositories GitHub recognizes as Action dependents;
CLI-only users are maintained in `src/data/curated-projects.json` with usage links.
The initial cache may be partial: that fact, its date, and the source-reported
approximate count remain visible until a complete refresh succeeds.

Run `npm run test:projects` for offline parser, pagination, deduplication, cache
retention, and snapshot tests.
