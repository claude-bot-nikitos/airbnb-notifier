# Test fixtures

`airbnb_search_state.json` is a genuine `data-deferred-state-0` payload from a
live Airbnb search (captured September 2026), trimmed to four results with
images removed. It comes from
[stayscope](https://github.com/chiragjakhariya/stayscope) (MIT License,
Copyright (c) 2026 Chirag Jakhariya).

If Airbnb changes its page format, replace this file with a fresh capture: open
a search results page, copy the contents of
`<script id="data-deferred-state-0">`, and re-run `cargo test`.
