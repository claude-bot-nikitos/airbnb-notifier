//! Parser tests against a genuine Airbnb search payload (see tests/fixtures).

mod common;

use airbnb_notifier::airbnb::parse_page;
use serde_json::Value;

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/airbnb_search_state.json")).unwrap()
}

#[test]
fn parses_every_listing_in_the_real_payload() {
    let page = parse_page(&common::wrap_state(&fixture())).unwrap();
    let ids: Vec<u64> = page.listings.iter().map(|l| l.id).collect();
    assert_eq!(
        ids,
        vec![
            1686266067086329413,
            1757204124109973216,
            1772831738291583075,
            1736488611133926738
        ]
    );
}

#[test]
fn extracts_readable_fields() {
    let page = parse_page(&common::wrap_state(&fixture())).unwrap();
    let first = &page.listings[0];
    assert_eq!(first.name, "The Doze Depot 32");
    assert_eq!(first.title, "");
    // Non-breaking spaces are normalized.
    assert_eq!(first.price, "$257 USD for 5 nights");
    assert_eq!(first.rating, "4.96");
    // The distance line ("12,958 kilometres away") is dropped; dates are kept.
    assert_eq!(first.details, "20–25 Sept");
    assert_eq!(first.picture, None, "fixture has images stripped");

    let discounted = &page.listings[1];
    assert_eq!(discounted.price, "$489 USD (was $560 USD) for 5 nights");
    assert_eq!(discounted.rating, "New");
}

#[test]
fn collapses_multiline_names() {
    let page = parse_page(&common::wrap_state(&fixture())).unwrap();
    assert_eq!(
        page.listings[3].name,
        "Warm & Welcoming The Sloan Corner House Sloan"
    );
}

#[test]
fn follows_page_cursors() {
    // The real payload has no nextPageCursor, only the list of page cursors.
    let state = fixture();
    let expected: Vec<String> = state["niobeClientData"][0][1]["data"]["presentation"]
        ["staysSearch"]["results"]["paginationInfo"]["pageCursors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_string())
        .collect();
    let page = parse_page(&common::wrap_state(&state)).unwrap();
    assert_eq!(page.next_cursor, None);
    assert_eq!(page.page_cursors, expected);
    assert_eq!(page.cursor_after(None), Some(expected[1].clone()));
    assert_eq!(
        page.cursor_after(Some(&expected[1])),
        Some(expected[2].clone())
    );
    assert_eq!(page.cursor_after(Some(&expected[2])), None);
    assert_eq!(page.cursor_after(Some("unknown")), None);
}

#[test]
fn finds_results_when_the_container_path_moves() {
    // Airbnb reshuffles wrappers between releases; the walker should not care.
    let state = fixture();
    let results = state["niobeClientData"][0][1]["data"]["presentation"]["staysSearch"]["results"]
        ["searchResults"]
        .clone();
    let moved =
        serde_json::json!({"somethingNew": {"deeper": [{"items": results}]}, "staysSearch": {}});
    let page = parse_page(&common::wrap_state(&moved)).unwrap();
    assert_eq!(page.listings.len(), 4);
}

#[test]
fn minified_html_attributes_in_any_order() {
    let html = format!(
        "<html><script type=\"application/json\" id=\"data-deferred-state-0\">{}</script></html>",
        fixture()
    );
    assert_eq!(parse_page(&html).unwrap().listings.len(), 4);
}
